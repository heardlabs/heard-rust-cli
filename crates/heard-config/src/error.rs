//! Errors are values.
//!
//! The Python module's discipline is "a failure is a sentence, never a crash":
//! a missing file is `{}`, a corrupt file is `{}` plus a stderr line plus a
//! rename. Only the cases where Python itself raises become `Err` here.

use std::path::PathBuf;

/// Everything `heard-config` can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `$HOME` is unset or empty, so no `~`-rooted path can be resolved.
    /// Python's `os.path.expanduser` falls back to the password database; we
    /// refuse instead of guessing, because every path this crate hands out is
    /// a place we are about to WRITE.
    #[error("cannot resolve the home directory: $HOME is unset or empty")]
    NoHome,

    /// A YAML layer parsed into something that is not a mapping.
    ///
    /// Python does `cfg.update(<whatever came back>)`, which raises
    /// `ValueError`/`TypeError` for a sequence or a scalar — `load()` dies.
    /// See `Config::load` for the one documented divergence here.
    #[error("{path}: config must be a mapping, found {found}")]
    NotAMapping {
        /// The file that parsed into a non-mapping.
        path: PathBuf,
        /// The YAML kind we got instead ("sequence", "string", …).
        found: &'static str,
    },

    /// A read/write/rename/lock failed.
    #[error("{path}: {source}")]
    Io {
        /// The file the operation was aimed at.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// `yaml.safe_load` raised something that is NOT a `yaml.YAMLError`
    /// (`ValueError` from `!!timestamp 2026-02-30`, `KeyError` from
    /// `!!bool maybe`, …). `config._read` only catches `YAMLError`, so in
    /// Python `load()` raises; so does this.
    #[error("{path}: {message}")]
    Load {
        /// The file that failed to load.
        path: PathBuf,
        /// What Python would have raised.
        message: String,
    },
}

impl ConfigError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        ConfigError::Io {
            path: path.into(),
            source,
        }
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, ConfigError>;
