//! The single source of truth for a project's spoken name.
//!
//! Ported from `heard/project_name.py`, whose docstring is the reasoning and is
//! kept here because it is the asset:
//!
//! > Every place that needs to say *which project* an agent is on — the
//! > narration project tag (`daemon._project_label`), the multi-agent session
//! > inference (`multi_agent._infer_repo_name`), and the raw per-session record
//! > (`agent_state`) — resolves through `canonical_project_name` here. One
//! > function, one rule:
//! >
//! > ```text
//! > git remote slug (owner/REPO -> REPO)  →  folder basename fallback
//! > ```
//! >
//! > The remote is what users actually *call* the project, and it stays correct
//! > even when the on-disk folder is stale — the classic case being a folder
//! > still named `old-project` while the repo's origin is `webapp`. Deriving
//! > the name from the folder is exactly why dead names kept resurfacing in
//! > narration no matter how many aliases or prompt notes were added: the name
//! > never came from anything a prompt could reach.
//!
//! The Python module mirrors the notch capturer's `_repo_name` /
//! `_git_remote_slug` (`heard-face/tools/capture.py`) so the notch and the
//! voice agree on the name. They can't share an import, so the algorithm is
//! kept identical instead — which now means in three places, not two.
//!
//! [`ProjectNamer`] exists because the git lookup is the one impure thing in an
//! otherwise deterministic layer. Production uses [`GitProjectNamer`]; the
//! golden corpus uses paths that are not repositories, where both namers agree
//! on the basename, so the fixtures do not depend on the machine that ran them.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a resolved remote stays cached. `git` is queried at most once per
/// path per TTL; every other call reads the cache instantly.
pub const REMOTE_TTL_S: f64 = 300.0;

/// How long `git remote get-url` gets before it is killed. Python passes
/// `timeout=3` for the same reason: a hung remote must never stall narration.
const GIT_TIMEOUT: Duration = Duration::from_secs(3);

/// Resolves a path to the project name a listener would use for it.
pub trait ProjectNamer: Send + Sync {
    /// `canonical_project_name`: git remote slug, else folder basename, else
    /// the path itself. Empty string for an empty path.
    fn canonical_project_name(&self, path: &str) -> String;
}

/// Folder basename only. Not a shortcut — it is exactly what the Python takes
/// when a path is not a git checkout, which is every path in the corpus.
#[derive(Debug, Default, Clone, Copy)]
pub struct BasenameProjectNamer;

impl ProjectNamer for BasenameProjectNamer {
    fn canonical_project_name(&self, path: &str) -> String {
        basename_fallback(path)
    }
}

/// The production namer: `git -C <path> remote get-url origin`, cached.
#[derive(Debug, Default)]
pub struct GitProjectNamer {
    cache: Mutex<HashMap<String, (Instant, Option<String>)>>,
}

impl GitProjectNamer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The repo's canonical name from its origin remote, or `None` when the
    /// path isn't a git repo / has no origin. Safe to call with any path inside
    /// the repo — `git -C` walks up to the real `.git`.
    pub fn git_remote_slug(&self, path: &str) -> Option<String> {
        if path.is_empty() {
            return None;
        }
        {
            let cache = self.cache.lock().expect("namer cache poisoned");
            if let Some((at, slug)) = cache.get(path) {
                if at.elapsed().as_secs_f64() < REMOTE_TTL_S {
                    return slug.clone();
                }
            }
        }
        let slug = run_git_remote(path);
        let mut cache = self.cache.lock().expect("namer cache poisoned");
        cache.insert(path.to_string(), (Instant::now(), slug.clone()));
        slug
    }
}

impl ProjectNamer for GitProjectNamer {
    fn canonical_project_name(&self, path: &str) -> String {
        if path.is_empty() {
            return String::new();
        }
        self.git_remote_slug(path)
            .unwrap_or_else(|| basename_fallback(path))
    }
}

/// `os.path.basename(path.rstrip("/")) or path`.
fn basename_fallback(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let trimmed = path.trim_end_matches('/');
    let base = Path::new(trimmed)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if base.is_empty() {
        path.to_string()
    } else {
        base.to_string()
    }
}

/// Spawn git with a hard deadline. Std has no `wait_timeout`, so this polls —
/// the call either answers in single-digit milliseconds or is a hung remote we
/// want gone, and polling keeps the whole module free of `unsafe` and of
/// dependencies the workspace does not already carry.
fn run_git_remote(path: &str) -> Option<String> {
    let mut child = Command::new("git")
        .args(["-C", path, "remote", "get-url", "origin"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + GIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return None,
        }
    }

    let output = child.wait_with_output().ok()?;
    let url = String::from_utf8_lossy(&output.stdout)
        .trim()
        .trim_end_matches('/')
        .to_string();
    if url.is_empty() {
        return None;
    }
    // …/owner/repo(.git)
    let slug = url.rsplit('/').next().unwrap_or("");
    let slug = slug.strip_suffix(".git").unwrap_or(slug);
    if slug.is_empty() {
        None
    } else {
        Some(slug.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_is_the_fallback_and_trailing_slashes_do_not_count() {
        let namer = BasenameProjectNamer;
        assert_eq!(namer.canonical_project_name("/Users/me/my-repo"), "my-repo");
        assert_eq!(
            namer.canonical_project_name("/Users/me/my-repo/"),
            "my-repo"
        );
        assert_eq!(namer.canonical_project_name(""), "");
        // A path that is nothing but separators has no basename; Python's
        // `or path` keeps the raw value rather than returning "".
        assert_eq!(namer.canonical_project_name("/"), "/");
    }

    #[test]
    fn git_namer_falls_back_to_the_basename_off_a_non_repo_path() {
        let namer = GitProjectNamer::new();
        // The path does not exist, so git exits non-zero and we take the
        // basename — the branch every corpus path travels.
        assert_eq!(
            namer.canonical_project_name("/nonexistent/heard-fixture/api"),
            "api"
        );
        // Second call is served from the cache.
        assert_eq!(
            namer.canonical_project_name("/nonexistent/heard-fixture/api"),
            "api"
        );
    }
}
