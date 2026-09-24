//! Careful file writes: a writer lock, a one-per-change backup, and
//! tmp + rename so a reader never sees half a file.
//!
//! Public so another edition embedding the core can reuse the same
//! machinery under its own names: every name that differs between editions
//! (the lock file, the backup infix, the temp-file tag) comes from a
//! [`FileNames`], and [`FileNames::CLI`] is exactly what heard-cli has
//! always used.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::InstallError;

/// The infix of every backup this crate makes: `settings.json.heard-cli-backup-<UTC>`.
pub const BACKUP_INFIX: &str = ".heard-cli-backup-";

/// How writers of one file keep out of each other's way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStyle {
    /// A lock file created exclusively and deleted on release; a file left
    /// by a crashed writer goes stale after 30 s. (heard-cli's.)
    CreateExclusive,
    /// An advisory `flock` on a lock file that stays on disk, the way
    /// Python's `fcntl.flock` writers do — so an edition can interoperate
    /// with an older writer that locks the same file that way.
    Flock,
}

/// How a rewritten JSON file is formatted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStyle {
    /// Two-space JSON, UTF-8 as-is, trailing newline. (heard-cli's.)
    Pretty,
    /// Python's `json.dump(data, f, indent=2)` + `"\n"`: ASCII-escaped,
    /// floats as `repr` — byte-identical to an older Python writer's file.
    PythonIndent2,
}

/// The per-edition names of the files this module makes beside a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileNames {
    /// Appended to the target's file name: `settings.json<lock_suffix>`.
    pub lock_suffix: &'static str,
    pub lock_style: LockStyle,
    /// `settings.json<backup_infix><UTC stamp>`.
    pub backup_infix: &'static str,
    /// The temp file is `.settings.json.<tmp_tag>-<pid>`.
    pub tmp_tag: &'static str,
    /// How [`crate::hooks::edit_hooks_file`] writes the file.
    pub style: WriteStyle,
}

impl FileNames {
    /// heard-cli's names.
    pub const CLI: FileNames = FileNames {
        lock_suffix: ".heard-cli.lock",
        lock_style: LockStyle::CreateExclusive,
        backup_infix: BACKUP_INFIX,
        tmp_tag: "heard-cli-tmp",
        style: WriteStyle::Pretty,
    };
}

pub(crate) fn io_err(path: &Path, source: io::Error) -> InstallError {
    InstallError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Write through a symlink (a dotfiles repo) instead of replacing it.
pub fn resolve_target(path: &Path) -> PathBuf {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
        }
        _ => path.to_owned(),
    }
}

/// Read a file, `None` if it does not exist.
pub fn read_opt(path: &Path) -> Result<Option<String>, InstallError> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(path, e)),
    }
}

/// An exclusive writer lock beside `path`, released on drop. Two installers
/// (or an installer and `heard doctor --fix`) never interleave a
/// read-modify-write of the same file.
pub struct Lock {
    path: PathBuf,
    /// `Some` for a [`LockStyle::Flock`] lock: the open, locked file. The
    /// lock file itself stays on disk.
    held: Option<fs::File>,
}

impl Lock {
    const WAIT: Duration = Duration::from_secs(3);
    const STALE: Duration = Duration::from_secs(30);

    /// heard-cli's lock ([`FileNames::CLI`]).
    pub fn acquire(path: &Path) -> Result<Self, InstallError> {
        Self::acquire_named(path, &FileNames::CLI)
    }

    /// The lock beside `path` under `names`. Gives up with
    /// [`InstallError::Locked`] after 3 s.
    pub fn acquire_named(path: &Path, names: &FileNames) -> Result<Self, InstallError> {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(names.lock_suffix);
        let lock = path.with_file_name(name);
        if let Some(dir) = lock.parent() {
            fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
        }
        let start = Instant::now();
        if names.lock_style == LockStyle::Flock {
            let f = OpenOptions::new()
                .append(true)
                .create(true)
                .open(&lock)
                .map_err(|e| io_err(&lock, e))?;
            loop {
                match f.try_lock() {
                    Ok(()) => {
                        return Ok(Self {
                            path: lock,
                            held: Some(f),
                        })
                    }
                    Err(fs::TryLockError::WouldBlock) => {
                        if start.elapsed() > Self::WAIT {
                            return Err(InstallError::Locked { path: lock });
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(fs::TryLockError::Error(e)) => return Err(io_err(&lock, e)),
                }
            }
        }
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&lock) {
                Ok(mut f) => {
                    let _ = write!(f, "{}", std::process::id());
                    return Ok(Self {
                        path: lock,
                        held: None,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&lock)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > Self::STALE);
                    if stale {
                        let _ = fs::remove_file(&lock);
                        continue;
                    }
                    if start.elapsed() > Self::WAIT {
                        return Err(InstallError::Locked { path: lock });
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(io_err(&lock, e)),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        match self.held.take() {
            // Closing the descriptor releases the flock; the file stays.
            Some(f) => drop(f),
            None => {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

/// Copy `path` to `<path>.heard-cli-backup-<UTC timestamp>` (0600: a
/// settings file can hold env values). Never overwrites an older backup.
pub fn backup(path: &Path) -> Result<PathBuf, InstallError> {
    backup_with(path, BACKUP_INFIX)
}

/// [`backup`] under another edition's infix.
pub fn backup_with(path: &Path, infix: &str) -> Result<PathBuf, InstallError> {
    let bytes = fs::read(path).map_err(|e| io_err(path, e))?;
    let stamp = utc_stamp(SystemTime::now());
    let base = format!(
        "{}{}{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        infix,
        stamp
    );
    for n in 0..1000 {
        let name = if n == 0 {
            base.clone()
        } else {
            format!("{base}-{n}")
        };
        let dest = path.with_file_name(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&dest)
        {
            Ok(mut f) => {
                f.write_all(&bytes).map_err(|e| io_err(&dest, e))?;
                f.sync_all().map_err(|e| io_err(&dest, e))?;
                return Ok(dest);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_err(&dest, e)),
        }
    }
    Err(io_err(
        path,
        io::Error::new(io::ErrorKind::AlreadyExists, "too many backups this second"),
    ))
}

/// Write `contents` to `path` atomically: a sibling temp file, fsync, rename.
/// An existing file's permission bits carry over.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), InstallError> {
    atomic_write_named(path, contents, &FileNames::CLI)
}

/// [`atomic_write`] with another edition's temp-file tag.
pub fn atomic_write_named(
    path: &Path,
    contents: &[u8],
    names: &FileNames,
) -> Result<(), InstallError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    let mode = fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o644);
    let tmp = dir.join(format!(
        ".{}.{}-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        names.tmp_tag,
        std::process::id()
    ));
    let result = (|| {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(contents)?;
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(io_err(path, e));
    }
    Ok(())
}

/// `20260923T101530Z`, without a date crate.
pub fn utc_stamp(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-to-civil.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Is `p` a regular file with an execute bit?
pub fn is_executable(p: &Path) -> bool {
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_formats_utc() {
        let t = UNIX_EPOCH + Duration::from_secs(1_790_158_530); // 2026-09-23T10:15:30Z
        assert_eq!(utc_stamp(t), "20260923T101530Z");
        assert_eq!(utc_stamp(UNIX_EPOCH), "19700101T000000Z");
    }
}
