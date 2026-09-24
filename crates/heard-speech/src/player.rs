//! Playback, behind a trait.
//!
//! The Python `Popen`s `afplay [-r rate] <file>` and waits, killing the child
//! for barge-in. That is [`AfplayPlayer`] here — compiled, and **never run by
//! any test in this workspace**: the owner's machine is live, and a test that
//! made a sound would be a test that talked over them. Every behavioural test
//! drives [`RecordingPlayer`], which records what it was asked to play and
//! honours cancellation the same way the real one does.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// One utterance's cancellation flag — the Python's per-item
/// `threading.Event`. Cheap to clone; clones share the flag.
#[derive(Debug, Clone, Default)]
pub struct Cancel {
    inner: Arc<CancelInner>,
}

#[derive(Debug, Default)]
struct CancelInner {
    flag: AtomicBool,
    notify: tokio::sync::Notify,
}

impl Cancel {
    /// A fresh, unset flag.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `cancel.set()`.
    pub fn set(&self) {
        self.inner.flag.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// `cancel.is_set()`.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.inner.flag.load(Ordering::SeqCst)
    }

    /// Resolves once the flag is set (immediately if it already is).
    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_set() {
                return;
            }
            notified.await;
        }
    }

    /// Whether two handles are the same flag (`self._current_cancel is cancel`).
    #[must_use]
    pub fn same(&self, other: &Cancel) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

/// How one playback ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayOutcome {
    /// Played to the end.
    Finished,
    /// Stopped because the cancel flag was set (barge-in, silence, mute).
    Cancelled,
    /// The player could not play it (non-zero exit, missing binary).
    Failed(String),
}

/// Something that plays an audio file to completion, or until cancelled.
/// Blocking: the queue runs it on the blocking pool.
pub trait Player: Send + Sync {
    /// Play `path` at `rate` (`1.0` = as synthesised). Must return promptly
    /// once `cancel` is set.
    fn play(&self, path: &Path, rate: f64, cancel: &Cancel) -> PlayOutcome;
}

/// `["afplay", path]`, or `["afplay", "-r", f"{rate:.3f}", path]` above the
/// backend's native speed — the exact argv the Python builds.
#[must_use]
pub fn afplay_args(path: &Path, rate: f64) -> Vec<String> {
    let path = path.to_string_lossy().into_owned();
    if (rate - 1.0).abs() > f64::EPSILON {
        vec!["-r".into(), format!("{rate:.3}"), path]
    } else {
        vec![path]
    }
}

/// The real player: `/usr/bin/afplay`. Not exercised by tests (see the module
/// docs); its argv is, through [`afplay_args`].
#[derive(Debug, Clone)]
pub struct AfplayPlayer {
    binary: PathBuf,
    poll: Duration,
}

impl Default for AfplayPlayer {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("/usr/bin/afplay"),
            poll: Duration::from_millis(50),
        }
    }
}

impl AfplayPlayer {
    /// `/usr/bin/afplay`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Player for AfplayPlayer {
    fn play(&self, path: &Path, rate: f64, cancel: &Cancel) -> PlayOutcome {
        if cancel.is_set() {
            return PlayOutcome::Cancelled;
        }
        let child = Command::new(&self.binary)
            .args(afplay_args(path, rate))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => return PlayOutcome::Failed(format!("spawning afplay: {e}")),
        };
        loop {
            if cancel.is_set() {
                // `_kill_current`: hard-kill so silence is instant.
                let _ = child.kill();
                let _ = child.wait();
                return PlayOutcome::Cancelled;
            }
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return PlayOutcome::Finished,
                Ok(Some(status)) => return PlayOutcome::Failed(format!("afplay exited {status}")),
                Ok(None) => std::thread::sleep(self.poll),
                Err(e) => return PlayOutcome::Failed(format!("waiting on afplay: {e}")),
            }
        }
    }
}

/// One call a [`RecordingPlayer`] received.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayCall {
    /// The file's bytes as text — the test TTS writes the words themselves,
    /// so this is what "would have been heard".
    pub content: String,
    /// The file extension (`.mp3`, `.wav`).
    pub ext: String,
    /// The `afplay -r` rate.
    pub rate: f64,
    /// How it ended.
    pub outcome: PlayOutcome,
}

#[derive(Debug, Default)]
struct Gate {
    hold: Option<String>,
    started: Vec<String>,
}

/// The test player. Plays nothing; records every call; can hold on a given
/// line (so a test can stuff the queue behind it, exactly the way the Python
/// tests pin their worker) and can take a fixed time per line.
#[derive(Debug, Default)]
pub struct RecordingPlayer {
    calls: Mutex<Vec<PlayCall>>,
    gate: Mutex<Gate>,
    cv: Condvar,
    per_play: Duration,
    fail_on: Mutex<Vec<String>>,
}

impl RecordingPlayer {
    /// Instant playback.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Each play takes `d` (still returning early on cancel).
    #[must_use]
    pub fn with_duration(d: Duration) -> Self {
        Self {
            per_play: d,
            ..Self::default()
        }
    }

    /// Block any play whose content is `text` until [`Self::release`] (or its
    /// cancel flag) — the Python tests' `proceed.wait()`.
    pub fn hold_on(&self, text: &str) {
        self.gate.lock().unwrap_or_else(|e| e.into_inner()).hold = Some(text.to_owned());
    }

    /// Let a held play finish.
    pub fn release(&self) {
        self.gate.lock().unwrap_or_else(|e| e.into_inner()).hold = None;
        self.cv.notify_all();
    }

    /// Make plays of `text` fail as a non-zero `afplay` exit would.
    pub fn fail_on(&self, text: &str) {
        self.fail_on
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(text.to_owned());
    }

    /// Wait until a play of `text` has STARTED.
    pub fn wait_started(&self, text: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        while !g.started.iter().any(|s| s == text) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            g = self
                .cv
                .wait_timeout(g, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    /// Every call so far.
    #[must_use]
    pub fn calls(&self) -> Vec<PlayCall> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The contents that played to the end, in order.
    #[must_use]
    pub fn finished(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c.outcome == PlayOutcome::Finished)
            .map(|c| c.content)
            .collect()
    }
}

impl Player for RecordingPlayer {
    fn play(&self, path: &Path, rate: f64, cancel: &Cancel) -> PlayOutcome {
        let content = std::fs::read(path)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        let ext = path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        {
            let mut g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            g.started.push(content.clone());
            self.cv.notify_all();
            while g.hold.as_deref() == Some(content.as_str()) && !cancel.is_set() {
                g = self
                    .cv
                    .wait_timeout(g, Duration::from_millis(5))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
        let deadline = Instant::now() + self.per_play;
        while Instant::now() < deadline && !cancel.is_set() {
            std::thread::sleep(Duration::from_millis(2));
        }
        let outcome = if cancel.is_set() {
            PlayOutcome::Cancelled
        } else if self
            .fail_on
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&content)
        {
            PlayOutcome::Failed("afplay exited 1".into())
        } else {
            PlayOutcome::Finished
        };
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(PlayCall {
                content,
                ext,
                rate,
                outcome: outcome.clone(),
            });
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn afplay_argv_matches_the_python() {
        let p = Path::new("/x/clip-1.mp3");
        assert_eq!(afplay_args(p, 1.0), vec!["/x/clip-1.mp3"]);
        assert_eq!(afplay_args(p, 1.5), vec!["-r", "1.500", "/x/clip-1.mp3"]);
        assert_eq!(
            afplay_args(p, 2.0 / 1.2),
            vec!["-r", "1.667", "/x/clip-1.mp3"]
        );
    }
}
