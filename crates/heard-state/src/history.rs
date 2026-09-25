//! Spoken history log.
//!
//! Append-only JSONL of every utterance the daemon spoke to completion. Drives
//! two consumers:
//!
//!   * `heard history`  — public read-only CLI for power users
//!   * `heard improve`  — owner-only judge loop that samples the log, asks
//!     Sonnet for tone/quality critique, and produces a markdown report for
//!     review
//!
//! Storage: `<CONFIG_DIR>/history.jsonl`. One JSON record per line. A sibling
//! `history.checkpoint` file holds the byte offset of the last entry consumed
//! by `heard improve`. After a successful improve run we truncate the file from
//! byte 0 up to the checkpoint, so the log doesn't accumulate forever — it's
//! meant to be ephemeral.
//!
//! Concurrency: the daemon is the sole writer (single process). Readers
//! (`heard history`, `heard improve`) are separate CLI invocations that open
//! the file read-only. We take an exclusive lock when truncating so a reader
//! doesn't see a half-truncated file.
//!
//! Privacy: strictly local. Nothing in this module touches the network.
//! `heard improve` is the only thing that does, and only when YOU run it.
//!
//! ## Byte-for-byte
//!
//! Records are [`PyValue`], not `serde_json::Value`, and are written through
//! [`PyValue::dumps`]. See `pyjson.rs` for why: CPython's separators, its
//! `ensure_ascii=False` UTF-8, and the insertion order of a record's keys are
//! all observable in a file users already have on disk and that `heard improve`
//! reads back.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::history_policy::HistoryPolicySource;
use crate::pyjson::PyValue;

/// Safety-net rotation. The intended pattern is `heard improve` pruning
/// consumed entries on every run, so the log stays small. This rotate-at-size
/// guard catches the case where the user has never run improve and the log
/// grows unbounded.
pub const ROTATE_BYTES: u64 = 50 * 1024 * 1024;

/// One history record, keys in insertion order.
pub type Record = Vec<(String, PyValue)>;

/// `time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())`.
pub fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_utc(secs as i64)
}

/// Seconds since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Howard Hinnant's civil-from-days, which is the same arithmetic `gmtime`
/// does and avoids pulling a date crate into a workspace that has none cached.
fn format_utc(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let secs_of_day = epoch_secs.rem_euclid(86_400);
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Stable random ID for one spoken utterance.
///
/// The daemon mints one on every speak request, stamps it into the history
/// record's `id` field, and remembers it as the most-recent utterance so a
/// later `heard feedback` / `heard report-defect` invocation can point at the
/// utterance it's about.
///
/// Feedback records are appended to the SAME history.jsonl as sibling lines
/// with `{"type": "feedback", "ref": <utterance_id>, ...}` — clean append-only,
/// no in-place rewrites of utterance records needed. Defects go to a separate
/// `defect_reports.jsonl` to keep the preference and defect flows cleanly
/// separated.
///
/// Shape-compatible with Python's `uuid.uuid4().hex`: 32 lowercase hex
/// characters with version 4 and the RFC 4122 variant bits set.
pub fn new_utterance_id() -> String {
    let mut bytes = [0u8; 16];
    // `read_exact`, never `fs::read`: `/dev/urandom` is an endless character
    // device, so reading it "to EOF" never returns.
    if let Ok(mut device) = File::open("/dev/urandom") {
        let _ = device.read_exact(&mut bytes);
    }
    if bytes.iter().all(|b| *b == 0) {
        // No /dev/urandom (shouldn't happen on macOS). Fall back to the clock
        // rather than mint a constant id, which would collapse every
        // utterance's feedback onto one record.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        bytes[..16].copy_from_slice(&nanos.to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The spoken-history log, rooted at a config directory.
#[derive(Clone)]
pub struct History {
    config_dir: PathBuf,
    /// The timestamp stamped onto a record that doesn't carry one. Injected so
    /// the golden corpus can pin exact bytes.
    clock: Arc<dyn Fn() -> String + Send + Sync>,
    /// What the log may keep of the user's words, read per append. `None` =
    /// [`crate::history_policy::HistoryPolicy::default`] (record everything).
    policy: Option<HistoryPolicySource>,
}

impl std::fmt::Debug for History {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("History")
            .field("config_dir", &self.config_dir)
            .field("policy", &self.policy.is_some())
            .finish_non_exhaustive()
    }
}

impl History {
    pub fn new(config_dir: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: config_dir.into(),
            clock: Arc::new(now_iso),
            policy: None,
        }
    }

    /// A log whose `ts` stamp comes from `clock`. Used by the corpus.
    pub fn with_clock(
        config_dir: impl Into<PathBuf>,
        clock: Arc<dyn Fn() -> String + Send + Sync>,
    ) -> Self {
        Self {
            config_dir: config_dir.into(),
            clock,
            policy: None,
        }
    }

    /// Read what the log may keep of the user's words from `policy`, on
    /// every append (see [`crate::history_policy`]).
    #[must_use]
    pub fn with_policy(mut self, policy: HistoryPolicySource) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn history_path(&self) -> PathBuf {
        self.config_dir.join("history.jsonl")
    }

    /// Byte offset into history.jsonl marking the last entry already consumed
    /// by an improve run. Anything before this offset is safe to prune;
    /// anything after is pending the next run.
    pub fn checkpoint_path(&self) -> PathBuf {
        self.config_dir.join("history.checkpoint")
    }

    /// Append one record to history.jsonl.
    ///
    /// Best-effort: if disk is full or the path is unwritable we silently drop
    /// — the daemon must never fail to speak because logging failed.
    pub fn append(&self, record: &Record) {
        let mut value = PyValue::Dict(record.clone());
        if let Some(policy) = &self.policy {
            policy().apply(&mut value);
        }
        value.set_default("ts", PyValue::Str((self.clock)()));
        let path = self.history_path();
        let _ = fs::create_dir_all(&self.config_dir);
        // One open-write-close per record. Cheap (~kB), keeps the
        // implementation simple, and means a reader sees consistent whole lines
        // at any time. No lock needed for appends — the OS guarantees
        // atomicity for writes ≤ PIPE_BUF.
        let written = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(format!("{}\n", value.dumps()).as_bytes()));
        if written.is_ok() {
            self.maybe_rotate();
        }
    }

    fn maybe_rotate(&self) {
        let path = self.history_path();
        let Ok(meta) = fs::metadata(&path) else {
            return;
        };
        if meta.len() > ROTATE_BYTES {
            let old = self.config_dir.join("history.jsonl.old");
            let _ = fs::remove_file(&old);
            let _ = fs::rename(&path, &old);
        }
    }

    /// Append a feedback record pointing at an utterance.
    ///
    /// Stored inline in history.jsonl as a sibling line with
    /// `type = "feedback"` so readers can filter cleanly. Best-effort: silently
    /// drops on write failure, like [`History::append`].
    pub fn append_feedback(&self, utterance_id: &str, source: &str, text: &str, kind: &str) {
        self.append(&vec![
            ("type".into(), PyValue::Str("feedback".into())),
            ("ref".into(), PyValue::Str(utterance_id.into())),
            ("kind".into(), PyValue::Str(kind.into())),
            ("source".into(), PyValue::Str(source.into())),
            ("text".into(), PyValue::Str(text.into())),
        ]);
    }

    /// Read every record after the saved checkpoint. Returns
    /// `(records, new_checkpoint_offset)`. Used by `heard improve`. Empty list
    /// when there's nothing new since the last run.
    pub fn iter_since_checkpoint(&self) -> (Vec<Record>, u64) {
        let path = self.history_path();
        if !path.exists() {
            return (Vec::new(), 0);
        }
        let mut start = self.read_checkpoint();
        let Ok(body) = fs::read(&path) else {
            return (Vec::new(), start);
        };
        let size = body.len() as u64;
        if start > size {
            // File got rotated / truncated under us; restart.
            start = 0;
        }
        let Ok(text) = std::str::from_utf8(&body[start as usize..]) else {
            return (Vec::new(), start);
        };
        (parse_lines(text), size)
    }

    /// Read the entire log (or the last `limit` entries). Used by
    /// `heard history`. No checkpoint side-effect.
    pub fn iter_all(&self, limit: Option<usize>) -> Vec<Record> {
        let Ok(text) = fs::read_to_string(self.history_path()) else {
            return Vec::new();
        };
        let mut out = parse_lines(&text);
        if let Some(limit) = limit {
            if limit > 0 && out.len() > limit {
                out = out.split_off(out.len() - limit);
            }
        }
        out
    }

    /// Two-step bookkeeping for a successful improve run:
    ///
    /// 1. Truncate history.jsonl from byte 0 up to `new_offset` — drops the
    ///    entries we just analysed so the log doesn't grow forever (user
    ///    explicitly asked for this).
    /// 2. Reset the checkpoint to 0 (since the file is now smaller).
    ///
    /// Concurrency: the daemon may be appending. We hold an exclusive lock
    /// during the rewrite so a concurrent append blocks rather than splicing
    /// into a half-truncated file.
    pub fn commit_checkpoint_and_prune(&self, new_offset: u64) {
        let path = self.history_path();
        if !path.exists() || new_offset == 0 {
            return;
        }
        let lock = OpenOptions::new().read(true).write(true).open(&path).ok();
        if let Some(file) = &lock {
            if file.lock().is_err() {
                return;
            }
        }
        // Read everything past new_offset, then rewrite the file with only
        // those bytes. Atomic-ish — a tmp file + rename, so a crash
        // mid-truncate leaves the original intact.
        let result = (|| -> std::io::Result<()> {
            let body = fs::read(&path)?;
            let tail = body.get(new_offset as usize..).unwrap_or_default().to_vec();
            let tmp = self.config_dir.join("history.jsonl.tmp");
            fs::write(&tmp, tail)?;
            fs::rename(&tmp, &path)?;
            Ok(())
        })();
        if result.is_ok() {
            self.write_checkpoint(0);
        }
        // On any failure leave the file alone — better to re-analyse the same
        // entries next run than to lose them.
        if let Some(file) = lock {
            let _ = file.unlock();
            drop::<File>(file);
        }
    }

    pub fn read_checkpoint(&self) -> u64 {
        fs::read_to_string(self.checkpoint_path())
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    }

    pub fn write_checkpoint(&self, offset: u64) {
        let _ = fs::create_dir_all(&self.config_dir);
        let _ = fs::write(self.checkpoint_path(), offset.to_string());
    }
}

/// Parse a JSONL body, skipping blank and unparseable lines exactly as the
/// Python `try: … except Exception: continue` does.
fn parse_lines(text: &str) -> Vec<Record> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| match PyValue::loads(line) {
            Some(PyValue::Dict(pairs)) => Some(pairs),
            // A non-object line parses in Python too and lands in the list as a
            // scalar; nothing in Heard has ever written one, and every reader
            // does `r["spoken"]`, so dropping it is the honest reading.
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_history(tag: &str) -> (History, PathBuf) {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "heard-state-history-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("tmp dir");
        let history = History::with_clock(&dir, Arc::new(|| "2026-09-22T12:00:00Z".to_string()));
        (history, dir)
    }

    fn spoken(text: &str) -> Record {
        vec![("spoken".into(), PyValue::Str(text.into()))]
    }

    #[test]
    fn format_utc_matches_strftime() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(1_000_000_000), "2001-09-09T01:46:40Z");
        // A leap day, which is where a hand-rolled calendar usually breaks.
        assert_eq!(format_utc(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(format_utc(1_790_000_000), "2026-09-21T14:13:20Z");
    }

    #[test]
    fn append_writes_one_line_per_call_with_a_stamped_ts() {
        // tests/test_history.py::test_append_writes_one_line_per_call
        let (history, dir) = temp_history("append");
        history.append(&vec![
            ("kind".into(), PyValue::Str("intermediate".into())),
            ("spoken".into(), PyValue::Str("first".into())),
        ]);
        history.append(&spoken("second"));
        let body = fs::read_to_string(history.history_path()).expect("read");
        assert_eq!(
            body,
            "{\"kind\": \"intermediate\", \"spoken\": \"first\", \"ts\": \"2026-09-22T12:00:00Z\"}\n\
             {\"spoken\": \"second\", \"ts\": \"2026-09-22T12:00:00Z\"}\n"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn checkpoint_walks_forward_and_survives_truncation() {
        // tests/test_history.py::test_iter_since_checkpoint_returns_only_new
        // and ::test_truncated_file_resets_checkpoint_gracefully
        let (history, dir) = temp_history("checkpoint");
        history.append(&spoken("a"));
        history.append(&spoken("b"));
        let (records, end) = history.iter_since_checkpoint();
        assert_eq!(records.len(), 2);
        assert!(end > 0);
        history.write_checkpoint(end);
        history.append(&spoken("c"));
        let (records, _) = history.iter_since_checkpoint();
        assert_eq!(records.len(), 1);

        history.write_checkpoint(end + 999_999);
        let (records, _) = history.iter_since_checkpoint();
        assert_eq!(records.len(), 3, "a checkpoint past EOF restarts from 0");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_drops_consumed_entries_and_keeps_later_ones() {
        // tests/test_history.py::test_commit_checkpoint_and_prune_truncates_consumed
        let (history, dir) = temp_history("prune");
        history.append(&spoken("old-1"));
        history.append(&spoken("old-2"));
        let (_, end) = history.iter_since_checkpoint();
        history.append(&spoken("new-after-improve-started"));
        history.commit_checkpoint_and_prune(end);
        let remaining: Vec<String> = history
            .iter_all(None)
            .into_iter()
            .filter_map(|r| {
                PyValue::Dict(r)
                    .get("spoken")
                    .and_then(PyValue::as_str)
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(remaining, vec!["new-after-improve-started".to_string()]);
        assert_eq!(history.read_checkpoint(), 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_fresh_install_reads_empty_rather_than_failing() {
        // tests/test_history.py::test_no_history_file_returns_empty
        let (history, dir) = temp_history("fresh");
        assert_eq!(history.iter_since_checkpoint(), (Vec::new(), 0));
        assert!(history.iter_all(None).is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn utterance_ids_are_unique_and_uuid4_shaped() {
        // tests/test_history.py::test_new_utterance_id_is_unique
        let a = new_utterance_id();
        let b = new_utterance_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(&a[12..13], "4", "version nibble");
        assert!(matches!(&a[16..17], "8" | "9" | "a" | "b"), "variant bits");
    }

    #[test]
    fn a_withholding_policy_keeps_no_user_words_on_disk() {
        let (history, dir) = temp_history("policy");
        let on = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&on);
        let history = history.with_policy(std::sync::Arc::new(move || {
            crate::history_policy::HistoryPolicy {
                record_user_text: flag.load(Ordering::SeqCst),
                ..Default::default()
            }
        }));
        history.append_feedback("u1", "cli", "SECRET-FEEDBACK words", "explicit");
        history.append(&vec![
            ("kind".into(), PyValue::Str("prompt_intent".into())),
            (
                "spoken".into(),
                PyValue::Str("SECRET-PROMPT restated".into()),
            ),
        ]);
        history.append(&spoken("Tests are green."));
        let body = fs::read_to_string(history.history_path()).expect("read");
        assert!(!body.contains("SECRET"), "{body}");
        assert!(body.contains("Tests are green."));
        assert_eq!(body.matches("\"redacted\": true").count(), 2, "{body}");
        // Re-read per append: flipping back on records again.
        on.store(true, Ordering::SeqCst);
        history.append_feedback("u1", "cli", "now recorded", "explicit");
        let body = fs::read_to_string(history.history_path()).expect("read");
        assert!(body.contains("now recorded"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_garbage_line_is_skipped_not_fatal() {
        let (history, dir) = temp_history("garbage");
        history.append(&spoken("good"));
        let mut file = OpenOptions::new()
            .append(true)
            .open(history.history_path())
            .expect("open");
        file.write_all(b"{not json\n\n").expect("write");
        history.append(&spoken("also good"));
        assert_eq!(history.iter_all(None).len(), 2);
        let _ = fs::remove_dir_all(dir);
    }
}
