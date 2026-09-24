//! The Kokoro voice model: download (resumable, verified, atomic), status,
//! remove.
//!
//! Source: the kokoro-onnx `model-files-v1.0` GitHub release, the same one
//! the Heard app uses. Each file is pinned by size (`heard_tts::select`) and by
//! SHA-256 (hashed from one download on 2026-09-23 and hard-coded here).
//!
//! For tests and mirrors, two environment variables override the source:
//! `HEARD_CLI_MODELS_URL` (base URL; the file name is appended) and
//! `HEARD_CLI_MODELS_MANIFEST` (a JSON file
//! `[{"name":…,"size":…,"sha256":…}]` replacing the pinned list).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use serde::{Deserialize, Serialize};

use crate::ui::{CliError, CliResult};

/// The release the files come from.
pub const DEFAULT_BASE_URL: &str =
    "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/";

/// SHA-256 of `kokoro-v1.0.onnx`.
pub const MODEL_SHA256: &str = "7d5df8ecf7d4b1878015a32686053fd0eebe2bc377234608764cc0ef3636a6c5";
/// SHA-256 of `voices-v1.0.bin`.
pub const VOICES_SHA256: &str = "bca610b8308e8d99f32e6fe4197e7ec01679264efed0cac9140fe9c29f1fbf7d";

/// Base-URL override.
pub const URL_ENV: &str = "HEARD_CLI_MODELS_URL";
/// Manifest override.
pub const MANIFEST_ENV: &str = "HEARD_CLI_MODELS_MANIFEST";

const FIX_RETRY: &str = "re-run `heard models download` — it resumes where it stopped";

/// One pinned file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFile {
    /// File name, also the URL's last segment.
    pub name: String,
    /// Exact byte size.
    pub size: u64,
    /// Lowercase hex SHA-256.
    pub sha256: String,
}

/// The pinned pair.
pub fn pinned() -> Vec<ModelFile> {
    vec![
        ModelFile {
            name: heard_tts::select::MODEL_FILE.into(),
            size: heard_tts::select::MODEL_SIZE,
            sha256: MODEL_SHA256.into(),
        },
        ModelFile {
            name: heard_tts::select::VOICES_FILE.into(),
            size: heard_tts::select::VOICES_SIZE,
            sha256: VOICES_SHA256.into(),
        },
    ]
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

/// One file's state on disk.
#[derive(Debug, Clone, Serialize)]
pub struct FileStatus {
    /// File name.
    pub name: String,
    /// Where it lives (or would).
    pub path: PathBuf,
    /// It exists.
    pub present: bool,
    /// Bytes on disk.
    pub size: u64,
    /// Bytes expected.
    pub expected_size: u64,
    /// SHA-256 matched (`None` = not checked).
    pub verified: Option<bool>,
    /// Bytes of a resumable partial download.
    pub partial_bytes: u64,
}

impl FileStatus {
    /// Present at the right size (and hash, if checked).
    pub fn ok(&self) -> bool {
        self.present && self.size == self.expected_size && self.verified != Some(false)
    }
}

fn part_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.part"))
}

/// Lowercase hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    Ok(ctx
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Status of every file; `verify` also hashes present, right-sized files.
pub fn status(dir: &Path, files: &[ModelFile], verify: bool) -> Vec<FileStatus> {
    files
        .iter()
        .map(|f| {
            let path = dir.join(&f.name);
            let meta = fs::metadata(&path).ok().filter(|m| m.is_file());
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let verified = (verify && meta.is_some() && size == f.size)
                .then(|| sha256_file(&path).map(|h| h == f.sha256).unwrap_or(false));
            FileStatus {
                name: f.name.clone(),
                present: meta.is_some(),
                size,
                expected_size: f.size,
                verified,
                partial_bytes: fs::metadata(part_path(dir, &f.name))
                    .map(|m| m.len())
                    .unwrap_or(0),
                path,
            }
        })
        .collect()
}

/// All files present at the right size (no hash): the fast check.
pub fn installed(dir: &Path, files: &[ModelFile]) -> bool {
    status(dir, files, false).iter().all(FileStatus::ok)
}

/// What happened to one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Outcome {
    /// Already there and verified.
    AlreadyPresent,
    /// Downloaded (bytes fetched this run, bytes resumed from a partial).
    Downloaded {
        /// Fetched now.
        fetched: u64,
        /// Picked up from a previous partial.
        resumed_from: u64,
    },
}

/// Download every missing file into `dir`.
pub fn download(src: &Source, dir: &Path, progress: bool) -> CliResult<Vec<(String, Outcome)>> {
    fs::create_dir_all(dir).map_err(|e| {
        CliError::failure(
            format!("cannot create {}: {e}", dir.display()),
            "check you can write to Heard's state directory (`heard config path`)",
        )
    })?;
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .build();
    let mut out = Vec::new();
    for f in &src.files {
        let o = download_one(&agent, src, f, dir, progress)?;
        out.push((f.name.clone(), o));
    }
    Ok(out)
}

fn download_one(
    agent: &ureq::Agent,
    src: &Source,
    f: &ModelFile,
    dir: &Path,
    progress: bool,
) -> CliResult<Outcome> {
    let dest = dir.join(&f.name);
    if let Ok(m) = fs::metadata(&dest) {
        if m.len() == f.size && sha256_file(&dest).is_ok_and(|h| h == f.sha256) {
            return Ok(Outcome::AlreadyPresent);
        }
        // Wrong size or hash: a truncated or corrupt install. Replace it.
        let _ = fs::remove_file(&dest);
    }
    let part = part_path(dir, &f.name);
    let mut offset = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if offset > f.size {
        let _ = fs::remove_file(&part);
        offset = 0;
    }
    let resumed_from = offset;
    let mut fetched = 0u64;

    if offset < f.size {
        let url = format!("{}{}", src.base_url, f.name);
        let mut req = agent.get(&url);
        if offset > 0 {
            req = req.set("Range", &format!("bytes={offset}-"));
        }
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(416, _)) => {
                // The server says the range starts past the end: the partial
                // is already whole. Fall through to verification.
                return finish(f, &part, &dest, fetched, resumed_from);
            }
            Err(e) => {
                return Err(CliError::failure(
                    format!("download of {} failed: {e}", f.name),
                    format!("check your connection, then {FIX_RETRY}"),
                ))
            }
        };
        let mut file = if resp.status() == 206 && offset > 0 {
            OpenOptions::new().append(true).open(&part)
        } else {
            // 200: the server ignored the range; start over.
            offset = 0;
            File::create(&part)
        }
        .map_err(|e| {
            CliError::failure(
                format!("cannot write {}: {e}", part.display()),
                "check free disk space and permissions on Heard's models directory",
            )
        })?;

        let bar = if progress {
            ProgressBar::new(f.size)
        } else {
            ProgressBar::with_draw_target(Some(f.size), ProgressDrawTarget::hidden())
        };
        if let Ok(style) = ProgressStyle::with_template(
            "{msg:18} [{bar:30}] {bytes}/{total_bytes} {bytes_per_sec} eta {eta}",
        ) {
            bar.set_style(style.progress_chars("=> "));
        }
        bar.set_message(f.name.clone());
        bar.set_position(offset);

        let mut reader = resp.into_reader();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    bar.abandon();
                    let _ = file.flush();
                    return Err(CliError::failure(
                        format!("download of {} interrupted: {e}", f.name),
                        FIX_RETRY,
                    ));
                }
            };
            if offset + n as u64 > f.size {
                bar.abandon();
                drop(file);
                let _ = fs::remove_file(&part);
                return Err(CliError::failure(
                    format!("{} is larger than the pinned {} bytes", f.name, f.size),
                    "the download source changed; re-run later or report it",
                ));
            }
            file.write_all(&buf[..n]).map_err(|e| {
                CliError::failure(
                    format!("cannot write {}: {e}", part.display()),
                    "check free disk space (the model needs about 354 MB)",
                )
            })?;
            offset += n as u64;
            fetched += n as u64;
            bar.set_position(offset);
        }
        file.sync_all().ok();
        bar.finish_and_clear();
    }
    finish(f, &part, &dest, fetched, resumed_from)
}

fn finish(
    f: &ModelFile,
    part: &Path,
    dest: &Path,
    fetched: u64,
    resumed_from: u64,
) -> CliResult<Outcome> {
    let got = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if got != f.size {
        return Err(CliError::failure(
            format!(
                "{}: got {got} of {} bytes (connection closed early)",
                f.name, f.size
            ),
            FIX_RETRY,
        ));
    }
    let hash = sha256_file(part).map_err(|e| {
        CliError::failure(format!("cannot read {}: {e}", part.display()), FIX_RETRY)
    })?;
    if hash != f.sha256 {
        let _ = fs::remove_file(part);
        return Err(CliError::failure(
            format!(
                "{}: checksum mismatch (expected {}, got {hash}); the bad file was deleted",
                f.name, f.sha256
            ),
            "re-run `heard models download`; if it repeats, your network is altering the download",
        ));
    }
    fs::rename(part, dest).map_err(|e| {
        CliError::failure(
            format!("cannot move {} into place: {e}", dest.display()),
            "check permissions on Heard's models directory",
        )
    })?;
    Ok(Outcome::Downloaded {
        fetched,
        resumed_from,
    })
}

/// Delete the model files and any partials. Returns what was removed.
pub fn remove(dir: &Path, files: &[ModelFile]) -> CliResult<Vec<PathBuf>> {
    let mut gone = Vec::new();
    for f in files {
        for p in [dir.join(&f.name), part_path(dir, &f.name)] {
            if p.exists() {
                fs::remove_file(&p).map_err(|e| {
                    CliError::failure(
                        format!("cannot remove {}: {e}", p.display()),
                        "check permissions on Heard's models directory",
                    )
                })?;
                gone.push(p);
            }
        }
    }
    Ok(gone)
}

/// Human size.
pub fn human(bytes: u64) -> String {
    let mb = bytes as f64 / 1_000_000.0;
    if mb >= 1.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{bytes} B")
    }
}
