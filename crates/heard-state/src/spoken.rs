//! Per-session tracking of which assistant-text blocks have already been
//! spoken, so we don't repeat them across PreToolUse / Stop events.
//!
//! Stored as a tiny JSON file under
//! `<CONFIG_DIR>/sessions/<session_id>.json` — just a list of recent text
//! hashes, capped to prevent unbounded growth.
//!
//! Hash collisions are not a security concern here — false positives just mean
//! a piece of text gets skipped. We use a 16-hex-char SHA-1 prefix, which is
//! more than enough for a single CC session's worth of messages.
//!
//! ## The lock
//!
//! Any per-session state is a flock'd read-modify-write, because concurrent
//! Claude Code and Codex sessions race otherwise. Concretely, without the lock two hooks both load the same
//! hashes, append different new ones, and only the last writer's set survives —
//! Heard then re-narrates the dropped block.
//!
//! The port uses [`std::fs::File::lock`], stabilised in Rust 1.89, which is
//! `flock(2)` on Unix — the same syscall `fcntl.flock` makes, on a sibling
//! `.lock` file kept separate so the lock's lifetime isn't entangled with the
//! JSON file's open/close cycle. That is why this crate needs neither `fs2` nor
//! `rustix` nor any `unsafe`.
//!
//! Best-effort, like the Python: if the lock can't be taken we proceed
//! unlocked rather than blocking the user's hook.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use crate::jsonl;
use crate::pyjson::PyValue;
use crate::sha1::sha1_hex;

/// Cap so the file never grows past a few KB even on long sessions.
pub const MAX_HASHES: usize = 500;

/// Sanitise a session id into a filename stem.
///
/// `re.sub(r"[^A-Za-z0-9_-]", "_", (session_id or "default")[:64]) or "default"`.
/// Note the truncation happens *before* the substitution and counts
/// characters, not bytes.
fn safe_stem(session_id: &str) -> String {
    let source = if session_id.is_empty() {
        "default"
    } else {
        session_id
    };
    let stem: String = source
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if stem.is_empty() {
        "default".to_string()
    } else {
        stem
    }
}

/// `hashlib.sha1(text.encode("utf-8", errors="replace")).hexdigest()[:16]`.
///
/// Rust strings are already valid UTF-8, so `errors="replace"` has nothing to
/// do — it exists on the Python side to survive a lone surrogate coming out of
/// a malformed transcript.
pub fn hash_text(text: &str) -> String {
    sha1_hex(text.as_bytes()).chars().take(16).collect()
}

/// A guard holding the per-session `flock`.
///
/// Dropping it releases the lock and closes the descriptor, which is the whole
/// of the Python `__exit__`.
struct SessionLock {
    file: Option<File>,
}

impl SessionLock {
    fn acquire(path: &Path) -> Self {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .ok();
        if let Some(file) = &file {
            // Blocking exclusive lock. A failure here is not fatal: the Python
            // "proceeds unlocked rather than blocking the user's hook", and so
            // do we.
            let _ = file.lock();
        }
        Self { file }
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

/// The per-session dedup store, rooted at a config directory.
///
/// The Python reads `config.CONFIG_DIR` at call time, which is how its tests
/// isolate. Here the directory is a field — same isolation, no global to
/// monkeypatch, and no way for a test to reach the real
/// `~/Library/Application Support/heard/` by forgetting a patch.
#[derive(Debug, Clone)]
pub struct SpokenStore {
    config_dir: PathBuf,
}

impl SpokenStore {
    pub fn new(config_dir: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: config_dir.into(),
        }
    }

    fn sessions_dir(&self) -> PathBuf {
        let dir = self.config_dir.join("sessions");
        let _ = fs::create_dir_all(&dir);
        dir
    }

    pub fn state_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir()
            .join(format!("{}.json", safe_stem(session_id)))
    }

    /// Lockfile sibling to the state file. Separate so flock semantics aren't
    /// entangled with the JSON file's open/close lifecycle.
    fn lock_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir()
            .join(format!("{}.lock", safe_stem(session_id)))
    }

    /// Sibling file holding the last byte offset we processed in the transcript
    /// JSONL. Lets PreToolUse / Stop hooks skip over lines they already parsed
    /// instead of re-walking the whole transcript on every event.
    pub fn offset_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir()
            .join(format!("{}.offset", safe_stem(session_id)))
    }

    fn load(&self, session_id: &str) -> Vec<String> {
        let path = self.state_path(session_id);
        let Ok(text) = fs::read_to_string(&path) else {
            return Vec::new();
        };
        let Some(value) = PyValue::loads(&text) else {
            return Vec::new();
        };
        match value.get("hashes").and_then(PyValue::as_list) {
            // `[str(h) for h in hashes]` — anything non-string is stringified
            // rather than dropped, so a hand-edited file can't silently lose
            // entries.
            Some(items) => items.iter().map(py_str).collect(),
            None => Vec::new(),
        }
    }

    fn save(&self, session_id: &str, hashes: &[String]) {
        let trimmed = if hashes.len() > MAX_HASHES {
            &hashes[hashes.len() - MAX_HASHES..]
        } else {
            hashes
        };
        let payload = PyValue::Dict(vec![(
            "hashes".to_string(),
            PyValue::List(trimmed.iter().cloned().map(PyValue::Str).collect()),
        )]);
        let path = self.state_path(session_id);
        // DELIBERATE DIVERGENCE (flagged in the port report, not a silent fix).
        //
        // Python does `p.write_text(...)`, which truncates and then writes. The
        // flock only guards writer-against-writer; `is_spoken`, `_load` and
        // `filter_unspoken` all read with NO lock. So a reader that lands
        // inside that window sees an empty or partial file, `json.loads`
        // raises, `_load` returns `[]` — and the text is narrated a second
        // time. That is precisely the bug the lock was added to prevent,
        // leaking back in through the read path.
        //
        // Writing to a sibling tmp file and renaming closes the window: rename
        // is atomic, so a reader sees either the whole previous file or the
        // whole new one. The final BYTES are identical, so no fixture and no
        // on-disk format changes — `tests/spoken_lock.rs` covers both halves.
        // Concurrent writers of the same session are already serialised by the
        // flock, so the tmp name cannot collide with itself.
        let tmp = path.with_extension("json.tmp");
        // Best-effort throughout: a failed write means one line may be
        // re-narrated, which is strictly better than a hook that errors.
        if fs::write(&tmp, payload.dumps()).is_ok() && fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }

    pub fn is_spoken(&self, session_id: &str, text: &str) -> bool {
        let hash = hash_text(text);
        self.load(session_id).contains(&hash)
    }

    pub fn mark_spoken(&self, session_id: &str, text: &str) {
        let hash = hash_text(text);
        let _guard = SessionLock::acquire(&self.lock_path(session_id));
        let mut hashes = self.load(session_id);
        if hashes.contains(&hash) {
            return;
        }
        hashes.push(hash);
        self.save(session_id, &hashes);
    }

    /// Return the subset of `texts` not yet marked spoken, preserving order.
    ///
    /// Does NOT mark them — call [`SpokenStore::mark_spoken`] after a
    /// successful send so we retry on failure.
    pub fn filter_unspoken(&self, session_id: &str, texts: &[String]) -> Vec<String> {
        let spoken = self.load(session_id);
        let mut seen_in_batch: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for text in texts {
            let hash = hash_text(text);
            if spoken.contains(&hash) || seen_in_batch.contains(&hash) {
                continue;
            }
            seen_in_batch.push(hash);
            out.push(text.clone());
        }
        out
    }

    /// Wipe state for a session (for tests / debugging).
    pub fn clear(&self, session_id: &str) {
        let _ = fs::remove_file(self.state_path(session_id));
        let _ = fs::remove_file(self.offset_path(session_id));
    }

    /// The last byte offset we've processed in the transcript. `0` if unknown —
    /// the caller falls back to a full read.
    pub fn get_offset(&self, session_id: &str) -> u64 {
        fs::read_to_string(self.offset_path(session_id))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }

    pub fn set_offset(&self, session_id: &str, offset: u64) {
        let _ = fs::write(self.offset_path(session_id), offset.to_string());
    }

    /// True iff we've already seen this session and recorded a byte offset.
    /// Used as the trigger for first-encounter EOF init.
    pub fn has_offset(&self, session_id: &str) -> bool {
        self.offset_path(session_id).exists()
    }

    /// First-encounter session init.
    ///
    /// Fresh installs / wiped state / never-before-seen sessions have no
    /// `.offset` file. Without this hook, the next transcript read starts at
    /// byte 0 and dumps every past assistant message and tool call into the
    /// speech queue — minutes or hours of replayed narration.
    ///
    /// This seeds:
    ///   * the dedup set with hashes of every assistant text already in the
    ///     transcript, so even if a later read parses old lines they won't be
    ///     narrated; and
    ///   * the byte offset at the current EOF, so the next incremental read
    ///     only picks up lines appended *after* this moment.
    ///
    /// Returns `true` if init ran (file was missing), `false` if state already
    /// existed and we left it alone. flock'd to stay consistent with the rest
    /// of the per-session state pattern.
    ///
    /// All filesystem failures are swallowed — if we can't read the transcript,
    /// the caller will fall through to the normal offset=0 read path and at
    /// worst replay history. That's the existing behaviour, so we're never
    /// worse off than today.
    pub fn initialize_at_eof(
        &self,
        session_id: &str,
        transcript_path: &Path,
        existing_texts: &[String],
    ) -> bool {
        let offset_path = self.offset_path(session_id);
        // Fast-path outside the lock: if the offset file is already there, init
        // already happened — nothing to do. (Re-checked inside the lock below
        // to avoid a race with a concurrent first hook.)
        if offset_path.exists() {
            return false;
        }

        let _guard = SessionLock::acquire(&self.lock_path(session_id));
        if offset_path.exists() {
            return false;
        }

        // Seed dedup hashes from any texts the caller already extracted, plus a
        // fresh scan of the transcript on disk. Both inputs are tolerated
        // empty.
        let mut seed_hashes: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for text in existing_texts {
            if text.is_empty() {
                continue;
            }
            let hash = hash_text(text);
            if seen.insert(hash.clone()) {
                seed_hashes.push(hash);
            }
        }

        // Streamed line by line — the transcript of a resumed long session is
        // tens of megabytes and must never be on the heap whole.
        // The hashes found are only kept if the whole file decoded, as the
        // old whole-file `read_to_string` did.
        let mut eof: u64 = 0;
        let mut from_transcript: Vec<String> = Vec::new();
        let mut undecodable = false;
        let read = jsonl::for_each_line_from(transcript_path, 0, jsonl::MAX_LINE_BYTES, |raw| {
            let Ok(line) = std::str::from_utf8(raw) else {
                undecodable = true;
                return ControlFlow::Break(());
            };
            if jsonl::definitely_not_type(line, "assistant") {
                return ControlFlow::Continue(());
            }
            let Some(message) = PyValue::loads(line) else {
                return ControlFlow::Continue(());
            };
            if message.get("type").and_then(PyValue::as_str) != Some("assistant") {
                return ControlFlow::Continue(());
            }
            let content = message
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(PyValue::as_list);
            for block in content.unwrap_or(&[]) {
                if block.get("type").and_then(PyValue::as_str) != Some("text") {
                    continue;
                }
                let text = block
                    .get("text")
                    .and_then(PyValue::as_str)
                    .unwrap_or("")
                    .trim();
                if text.is_empty() {
                    continue;
                }
                let hash = hash_text(text);
                if seen.insert(hash.clone()) {
                    from_transcript.push(hash);
                }
            }
            ControlFlow::Continue(())
        });
        if let (Ok(end), false) = (read, undecodable) {
            seed_hashes.extend(from_transcript);
            // Python takes `f.tell()` after consuming every line, which for a
            // fully-read file is its size.
            eof = end;
        }

        // Merge with whatever was already in the dedup file (normally empty on
        // first encounter, but defensive against a stray `.json` left by an
        // older Heard with no matching `.offset`).
        let prior = self.load(session_id);
        let mut merged = prior.clone();
        for hash in seed_hashes {
            if !merged.contains(&hash) {
                merged.push(hash);
            }
        }
        self.save(session_id, &merged);

        if fs::write(&offset_path, eof.to_string()).is_err() {
            return false;
        }
        true
    }
}

/// `str(value)` for the handful of JSON scalars a hand-edited hash list could
/// contain.
fn py_str(value: &PyValue) -> String {
    match value {
        PyValue::Str(s) => s.clone(),
        PyValue::Null => "None".to_string(),
        PyValue::Bool(true) => "True".to_string(),
        PyValue::Bool(false) => "False".to_string(),
        other => other.dumps(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_store(tag: &str) -> (SpokenStore, PathBuf) {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "heard-state-spoken-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("tmp dir");
        (SpokenStore::new(&dir), dir)
    }

    #[test]
    fn hash_matches_pythons_sha1_prefix() {
        assert_eq!(hash_text(""), "da39a3ee5e6b4b0d");
        assert_eq!(hash_text("hello world"), "2aae6c35c94fcfb4");
    }

    #[test]
    fn session_ids_are_sanitised_truncated_and_never_empty() {
        assert_eq!(safe_stem("abc"), "abc");
        assert_eq!(safe_stem(""), "default");
        assert_eq!(safe_stem("a/b/c"), "a_b_c");
        assert_eq!(safe_stem(".."), "__");
        assert_eq!(safe_stem("ünïcode"), "_n_code");
        assert_eq!(safe_stem(&"x".repeat(200)).len(), 64);
    }

    #[test]
    fn mark_then_filter_round_trips() {
        let (store, dir) = temp_store("round");
        assert!(!store.is_spoken("s1", "alpha"));
        store.mark_spoken("s1", "alpha");
        assert!(store.is_spoken("s1", "alpha"));
        let texts: Vec<String> = ["alpha", "fresh", "fresh", "other"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            store.filter_unspoken("s1", &texts),
            vec!["fresh".to_string(), "other".to_string()]
        );
        // filter does not mark, so a retry sees the same set.
        assert_eq!(store.filter_unspoken("s1", &texts).len(), 2);
        store.clear("s1");
        assert!(!store.is_spoken("s1", "alpha"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupt_state_file_reads_as_empty_rather_than_failing() {
        let (store, dir) = temp_store("corrupt");
        store.mark_spoken("s1", "alpha");
        fs::write(store.state_path("s1"), "{not json").expect("write");
        assert!(!store.is_spoken("s1", "alpha"));
        // …and the next write repairs it.
        store.mark_spoken("s1", "alpha");
        assert!(store.is_spoken("s1", "alpha"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn offsets_round_trip_and_default_to_zero() {
        let (store, dir) = temp_store("offset");
        assert!(!store.has_offset("s1"));
        assert_eq!(store.get_offset("s1"), 0);
        store.set_offset("s1", 4096);
        assert!(store.has_offset("s1"));
        assert_eq!(store.get_offset("s1"), 4096);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_lock_is_exclusive_between_two_handles() {
        // Not a race test (that is tests/spoken_lock.rs) — this only proves the
        // guard really takes the OS lock, so a second acquire has to wait.
        let (store, dir) = temp_store("lockheld");
        let path = store.lock_path("s1");
        let guard = SessionLock::acquire(&path);
        let probe = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .expect("open probe");
        assert!(
            probe.try_lock().is_err() || probe.try_lock().is_ok_and(|_| false),
            "a second exclusive lock should not be available while one is held"
        );
        drop(guard);
        assert!(probe.try_lock().is_ok(), "lock should be free after drop");
        let _ = probe.unlock();
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_to_state_file_is_the_exact_python_shape() {
        let (store, dir) = temp_store("shape");
        store.mark_spoken("s1", "alpha");
        let body = fs::read_to_string(store.state_path("s1")).expect("read");
        assert_eq!(body, r#"{"hashes": ["be76331b95dfc399"]}"#);
        let _ = fs::remove_dir_all(dir);
    }
}
