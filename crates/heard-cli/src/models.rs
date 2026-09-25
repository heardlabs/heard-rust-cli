//! The Kokoro voice model: download (resumable, verified, atomic), status,
//! remove — the CLI's face on [`heard_tts::download`], which owns the
//! mechanics (the app edition downloads through the same code).
//!
//! Source: the kokoro-onnx `model-files-v1.0` GitHub release, the same one
//! the Heard app uses. Each file is pinned by size (`heard_tts::select`) and by
//! SHA-256 (`heard_tts::download`).
//!
//! For tests and mirrors, two environment variables override the source:
//! `HEARD_CLI_MODELS_URL` (base URL; the file name is appended) and
//! `HEARD_CLI_MODELS_MANIFEST` (a JSON file
//! `[{"name":…,"size":…,"sha256":…}]` replacing the pinned list).

use std::fs;
use std::path::{Path, PathBuf};

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

pub use heard_tts::download::{
    human, installed, pinned, sha256_file, status, FileStatus, ModelFile, Outcome,
    DEFAULT_BASE_URL, MODEL_SHA256, VOICES_SHA256,
};

use crate::ui::{CliError, CliResult};

/// Base-URL override.
pub const URL_ENV: &str = "HEARD_CLI_MODELS_URL";
/// Manifest override.
pub const MANIFEST_ENV: &str = "HEARD_CLI_MODELS_MANIFEST";

/// How the CLI tells the user to retry.
const RETRY: &str = "re-run `heard models download`";

fn cli_err(e: heard_tts::download::DownloadError) -> CliError {
    CliError::failure(e.message, e.fix)
}

/// Where to download from and what to expect.
#[derive(Debug, Clone)]
pub struct Source {
    /// Base URL ending in `/`.
    pub base_url: String,
    /// The files.
    pub files: Vec<ModelFile>,
}

impl Source {
    /// The pinned source, unless the environment overrides it.
    pub fn from_env() -> CliResult<Self> {
        let mut base_url = std::env::var(URL_ENV)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.into());
        if !base_url.ends_with('/') {
            base_url.push('/');
        }
        let files = match std::env::var_os(MANIFEST_ENV).filter(|s| !s.is_empty()) {
            None => pinned(),
            Some(p) => {
                let text = fs::read_to_string(&p).map_err(|e| {
                    CliError::failure(
                        format!(
                            "cannot read {MANIFEST_ENV} file {}: {e}",
                            Path::new(&p).display()
                        ),
                        format!("unset {MANIFEST_ENV} to use the pinned Kokoro files"),
                    )
                })?;
                serde_json::from_str(&text).map_err(|e| {
                    CliError::failure(
                        format!("{MANIFEST_ENV} is not a valid manifest: {e}"),
                        format!("unset {MANIFEST_ENV} to use the pinned Kokoro files"),
                    )
                })?
            }
        };
        Ok(Source { base_url, files })
    }
}

/// Download every missing file into `dir`.
pub fn download(src: &Source, dir: &Path, progress: bool) -> CliResult<Vec<(String, Outcome)>> {
    let core = heard_tts::download::Source {
        base_url: src.base_url.clone(),
        files: src.files.clone(),
        retry: RETRY.into(),
    };
    let mut bar = Bar {
        bar: None,
        visible: progress,
    };
    heard_tts::download::download(&core, dir, &mut bar).map_err(cli_err)
}

/// The indicatif bar over [`heard_tts::download::Progress`].
struct Bar {
    bar: Option<ProgressBar>,
    visible: bool,
}

impl heard_tts::download::Progress for Bar {
    fn start(&mut self, name: &str, total: u64, at: u64) {
        let bar = if self.visible {
            ProgressBar::new(total)
        } else {
            ProgressBar::with_draw_target(Some(total), ProgressDrawTarget::hidden())
        };
        if let Ok(style) = ProgressStyle::with_template(
            "{msg:18} [{bar:30}] {bytes}/{total_bytes} {bytes_per_sec} eta {eta}",
        ) {
            bar.set_style(style.progress_chars("=> "));
        }
        bar.set_message(name.to_string());
        bar.set_position(at);
        self.bar = Some(bar);
    }
    fn advance(&mut self, pos: u64) {
        if let Some(b) = &self.bar {
            b.set_position(pos);
        }
    }
    fn finish(&mut self) {
        if let Some(b) = self.bar.take() {
            b.finish_and_clear();
        }
    }
    fn abandon(&mut self) {
        if let Some(b) = self.bar.take() {
            b.abandon();
        }
    }
}

/// Delete the model files and any partials. Returns what was removed.
pub fn remove(dir: &Path, files: &[ModelFile]) -> CliResult<Vec<PathBuf>> {
    heard_tts::download::remove(dir, files).map_err(cli_err)
}
