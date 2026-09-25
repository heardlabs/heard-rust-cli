//! The Kokoro model files: download (resumable, verified, atomic), status
//! and removal — `KokoroTTS.ensure_downloaded` in `engine/heard/tts/kokoro.py`,
//! with the hardening the CLI added (pinned SHA-256, `Range` resume, a size
//! cap so a changed source can never fill the disk).
//!
//! Not behind the `kokoro` feature: fetching and checking two files needs no
//! ONNX Runtime, so a build without the local voice can still show "the voice
//! is downloaded" and fetch it for a later build that has it.
//!
//! Nothing here runs by itself. The backend ([`crate::kokoro::KokoroTts`])
//! only ever READS these files; a download is always an explicit call by the
//! program's "download the voice" command, because a library that quietly
//! pulls 325 MB is how that became a bug in the first place.
//!
//! Source: the kokoro-onnx `model-files-v1.0` GitHub release, the same one
//! the Python app uses. Each file is pinned by size ([`crate::select`]) and by
//! SHA-256 (hashed from one download on 2026-09-23).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The release the files come from.
pub const DEFAULT_BASE_URL: &str =
    "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/";

/// SHA-256 of `kokoro-v1.0.onnx`.
pub const MODEL_SHA256: &str = "7d5df8ecf7d4b1878015a32686053fd0eebe2bc377234608764cc0ef3636a6c5";
/// SHA-256 of `voices-v1.0.bin`.
pub const VOICES_SHA256: &str = "bca610b8308e8d99f32e6fe4197e7ec01679264efed0cac9140fe9c29f1fbf7d";

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
#[must_use]
pub fn pinned() -> Vec<ModelFile> {
    vec![
        ModelFile {
            name: crate::select::MODEL_FILE.into(),
            size: crate::select::MODEL_SIZE,
            sha256: MODEL_SHA256.into(),
        },
        ModelFile {
            name: crate::select::VOICES_FILE.into(),
            size: crate::select::VOICES_SIZE,
            sha256: VOICES_SHA256.into(),
        },
    ]
}

/// A download failed: what happened, and what the user can do about it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DownloadError {
    /// The sentence.
    pub message: String,
    /// The way out.
    pub fix: String,
}

impl DownloadError {
    fn new(message: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fix: fix.into(),
        }
    }
}

/// Where the bytes go while they arrive. Every method has a no-op default,
/// so [`NoProgress`] is just `impl Progress for NoProgress {}`.
pub trait Progress {
    /// A file starts: `total` bytes expected, `at` already on disk.
    fn start(&mut self, name: &str, total: u64, at: u64) {
        let _ = (name, total, at);
    }
    /// `pos` bytes of the current file are on disk.
    fn advance(&mut self, pos: u64) {
        let _ = pos;
    }
    /// The current file finished streaming.
    fn finish(&mut self) {}
    /// The current file stopped with an error.
    fn abandon(&mut self) {}
}

/// Report nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoProgress;
impl Progress for NoProgress {}

/// Where to download from, what to expect, and how to tell the user to
/// retry (a CLI says "re-run `heard models download`", an app "try again").
#[derive(Debug, Clone)]
pub struct Source {
    /// Base URL ending in `/` (the file name is appended).
    pub base_url: String,
    /// The files.
    pub files: Vec<ModelFile>,
    /// The retry instruction, e.g. "re-run `heard models download`".
    pub retry: String,
}

impl Source {
    /// The pinned files from the pinned release.
    #[must_use]
    pub fn pinned(retry: &str) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            files: pinned(),
            retry: retry.into(),
        }
    }

    /// Normalise `base_url` to end in `/`.
    #[must_use]
    pub fn with_base_url(mut self, base_url: &str) -> Self {
        let mut b = base_url.trim().to_string();
        if !b.ends_with('/') {
            b.push('/');
        }
        self.base_url = b;
        self
    }

    fn resume_hint(&self) -> String {
        format!("{} — it resumes where it stopped", self.retry)
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
    #[must_use]
    pub fn ok(&self) -> bool {
        self.present && self.size == self.expected_size && self.verified != Some(false)
    }
}

fn part_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.part"))
}

/// Lowercase hex SHA-256 of a file.
///
/// # Errors
/// Reading the file.
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
#[must_use]
pub fn status(dir: &Path, files: &[ModelFile], verify: bool) -> Vec<FileStatus> {
    files
        .iter()
        .map(|f| {
            let path = dir.join(&f.name);
            let meta = fs::metadata(&path).ok().filter(fs::Metadata::is_file);
            let size = meta.as_ref().map_or(0, fs::Metadata::len);
            let verified = (verify && meta.is_some() && size == f.size)
                .then(|| sha256_file(&path).is_ok_and(|h| h == f.sha256));
            FileStatus {
                name: f.name.clone(),
                present: meta.is_some(),
                size,
                expected_size: f.size,
                verified,
                partial_bytes: fs::metadata(part_path(dir, &f.name)).map_or(0, |m| m.len()),
                path,
            }
        })
        .collect()
}

/// All files present at the right size (no hash): the fast check.
#[must_use]
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
///
/// # Errors
/// The first file that cannot be fetched, written or verified.
pub fn download(
    src: &Source,
    dir: &Path,
    progress: &mut dyn Progress,
) -> Result<Vec<(String, Outcome)>, DownloadError> {
    fs::create_dir_all(dir).map_err(|e| {
        DownloadError::new(
            format!("cannot create {}: {e}", dir.display()),
            "check you can write to Heard's state directory",
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
    progress: &mut dyn Progress,
) -> Result<Outcome, DownloadError> {
    let dest = dir.join(&f.name);
    if let Ok(m) = fs::metadata(&dest) {
        if m.len() == f.size && sha256_file(&dest).is_ok_and(|h| h == f.sha256) {
            return Ok(Outcome::AlreadyPresent);
        }
        // Wrong size or hash: a truncated or corrupt install. Replace it.
        let _ = fs::remove_file(&dest);
    }
    let part = part_path(dir, &f.name);
    let mut offset = fs::metadata(&part).map_or(0, |m| m.len());
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
                return finish(src, f, &part, &dest, fetched, resumed_from);
            }
            Err(e) => {
                return Err(DownloadError::new(
                    format!("download of {} failed: {e}", f.name),
                    format!("check your connection, then {}", src.resume_hint()),
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
            DownloadError::new(
                format!("cannot write {}: {e}", part.display()),
                "check free disk space and permissions on Heard's models directory",
            )
        })?;

        progress.start(&f.name, f.size, offset);
        let mut reader = resp.into_reader();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    progress.abandon();
                    let _ = file.flush();
                    return Err(DownloadError::new(
                        format!("download of {} interrupted: {e}", f.name),
                        src.resume_hint(),
                    ));
                }
            };
            if offset + n as u64 > f.size {
                progress.abandon();
                drop(file);
                let _ = fs::remove_file(&part);
                return Err(DownloadError::new(
                    format!("{} is larger than the pinned {} bytes", f.name, f.size),
                    "the download source changed; re-run later or report it",
                ));
            }
            file.write_all(&buf[..n]).map_err(|e| {
                DownloadError::new(
                    format!("cannot write {}: {e}", part.display()),
                    "check free disk space (the model needs about 354 MB)",
                )
            })?;
            offset += n as u64;
            fetched += n as u64;
            progress.advance(offset);
        }
        file.sync_all().ok();
        progress.finish();
    }
    finish(src, f, &part, &dest, fetched, resumed_from)
}

fn finish(
    src: &Source,
    f: &ModelFile,
    part: &Path,
    dest: &Path,
    fetched: u64,
    resumed_from: u64,
) -> Result<Outcome, DownloadError> {
    let got = fs::metadata(part).map_or(0, |m| m.len());
    if got != f.size {
        return Err(DownloadError::new(
            format!(
                "{}: got {got} of {} bytes (connection closed early)",
                f.name, f.size
            ),
            src.resume_hint(),
        ));
    }
    let hash = sha256_file(part).map_err(|e| {
        DownloadError::new(
            format!("cannot read {}: {e}", part.display()),
            src.resume_hint(),
        )
    })?;
    if hash != f.sha256 {
        let _ = fs::remove_file(part);
        return Err(DownloadError::new(
            format!(
                "{}: checksum mismatch (expected {}, got {hash}); the bad file was deleted",
                f.name, f.sha256
            ),
            format!(
                "{}; if it repeats, your network is altering the download",
                src.retry
            ),
        ));
    }
    fs::rename(part, dest).map_err(|e| {
        DownloadError::new(
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
///
/// # Errors
/// A file that exists but cannot be removed.
pub fn remove(dir: &Path, files: &[ModelFile]) -> Result<Vec<PathBuf>, DownloadError> {
    let mut gone = Vec::new();
    for f in files {
        for p in [dir.join(&f.name), part_path(dir, &f.name)] {
            if p.exists() {
                fs::remove_file(&p).map_err(|e| {
                    DownloadError::new(
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
#[must_use]
pub fn human(bytes: u64) -> String {
    let mb = bytes as f64 / 1_000_000.0;
    if mb >= 1.0 {
        format!("{mb:.0} MB")
    } else {
        format!("{bytes} B")
    }
}
