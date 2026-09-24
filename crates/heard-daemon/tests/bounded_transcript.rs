//! Regression test for an unbounded-memory bug: the hook path
//! must not put the agent's transcript on the heap whole.
//!
//! The e2e stream — 401 hook events over a 36–38 MB Claude Code
//! transcript — took the daemon to 1,161 MB max RSS (Python: 67 MB), because
//! every PreToolUse / Stop did `fs::read_to_string` of the entire transcript
//! (and a full forward parse for "the last assistant text"), even though the
//! reads claimed to be incremental.
//!
//! This binary installs a counting global allocator and drives the real
//! per-event entry point, `hooks::process`, over a ~40 MB synthetic
//! transcript. It asserts the PEAK live heap growth across every event stays
//! a small fraction of the transcript's size — the old code's floor was one
//! full copy of the file per read. It also asserts the narration still comes
//! out, so the bound cannot be met by reading nothing.
//!
//! One `#[test]` only: the allocator counters are process-global, and a
//! second test running in parallel would pollute the peak.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use heard_daemon::{hooks, testing, transcript, DaemonBuilder, LogSpeech};
use serde_json::{json, Value};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                let now = CURRENT.fetch_add(new_size - layout.size(), Ordering::Relaxed)
                    + (new_size - layout.size());
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Run `f` and return the peak live-heap growth (bytes) it caused.
fn peak_growth<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed).saturating_sub(base))
}

const MB: usize = 1024 * 1024;
/// One tool result per block: a big `"type":"user"` line, the shape that
/// dominates a real transcript's bytes.
const TOOL_RESULT_BYTES: usize = 256 * 1024;
const BLOCKS: usize = 160; // ~40 MB

fn user_line(i: usize) -> String {
    let body = format!("{i:08} ").repeat(TOOL_RESULT_BYTES / 9);
    json!({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": format!("t{i}"), "content": body}
    ]}})
    .to_string()
}

fn assistant_line(text: &str) -> String {
    json!({"type": "assistant", "message": {"content": [
        {"type": "text", "text": text},
        {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}}
    ]}})
    .to_string()
}

fn append(path: &Path, lines: &[String]) {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open");
    for line in lines {
        f.write_all(line.as_bytes()).expect("write");
        f.write_all(b"\n").expect("write");
    }
}

fn payload(event: &str, transcript: &Path) -> Value {
    json!({
        "hook_event_name": event,
        "session_id": "b1-bounded",
        "transcript_path": transcript.to_string_lossy(),
        "cwd": "/tmp/b1-bounded-project",
        "tool_name": "Bash",
        "tool_input": {"command": "ls", "description": "Listing files"},
        "tool_response": {"stdout": "a\nb", "stderr": "", "interrupted": false},
    })
}

#[test]
fn hook_events_over_a_large_transcript_stay_within_a_bounded_heap() {
    let root = testing::temp_dir("b1-bounded");
    let paths = heard_config::Paths::under(&root);
    fs::create_dir_all(&paths.config_dir).expect("config dir");
    // No flush wait: the test is about memory, not the agent's fsync timing.
    // Onboarded: since narration_policy was ported, a fresh install is held by
    // first run exactly as Python holds it, so nothing would reach the sink.
    fs::write(
        paths.config_dir.join("config.yaml"),
        "onboarded: true\nflush_delay_ms: 0\n",
    )
    .expect("config");
    let speech_log = root.join("speech.jsonl");
    let daemon = DaemonBuilder::new(paths)
        .speech(Arc::new(LogSpeech::new(&speech_log)))
        .build();

    let tr = root.join("transcript.jsonl");
    let mut history = Vec::new();
    for i in 0..BLOCKS {
        history.push(assistant_line(&format!(
            "Historical step number {i} is done and dusted."
        )));
        history.push(user_line(i));
    }
    append(&tr, &history);
    drop(history);
    let size = fs::metadata(&tr).expect("stat").len() as usize;
    assert!(size > 38 * MB, "synthetic transcript is {size} bytes");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");

    // The old code's floor was one whole copy of the transcript per read;
    // the bound is an eighth of it (lines here are 256 KB).
    let bound = size / 8;

    // First encounter: initialise at EOF (a full, streamed scan).
    let ((), grew) = peak_growth(|| {
        rt.block_on(hooks::process(
            &daemon,
            "claude-code",
            &payload("PreToolUse", &tr),
            None,
        ));
    });
    assert!(
        grew < bound,
        "first-encounter init grew the heap by {grew} bytes (bound {bound})"
    );

    let mut worst = 0usize;
    for round in 0..6 {
        let said = format!("Round {round} finished; the new module compiles cleanly now.");
        append(&tr, &[assistant_line(&said), user_line(10_000 + round)]);
        // Stop narrates the new prose as a final (an intermediate would be
        // floor-dropped); the PreToolUse after it finds no new prose and
        // takes the tool_pre path, which looks up the last assistant text.
        for event in ["Stop", "PreToolUse", "PostToolUse"] {
            let ((), grew) = peak_growth(|| {
                rt.block_on(hooks::process(
                    &daemon,
                    "claude-code",
                    &payload(event, &tr),
                    None,
                ));
            });
            worst = worst.max(grew);
        }
    }
    assert!(
        worst < bound,
        "a hook event grew the heap by {worst} bytes (bound {bound})"
    );

    // The helpers on their own, over the whole file.
    let (last, grew) = peak_growth(|| transcript::extract_last_assistant_text(&tr));
    assert_eq!(
        last,
        "Round 5 finished; the new module compiles cleanly now."
    );
    assert!(
        grew < 2 * MB,
        "last-assistant-text grew the heap by {grew} bytes"
    );
    let ((texts, end), grew) = peak_growth(|| transcript::extract_assistant_texts_from(&tr, 0));
    assert_eq!(texts.len(), BLOCKS + 6);
    assert_eq!(end, fs::metadata(&tr).expect("stat").len());
    assert!(
        grew < 2 * MB,
        "a full forward read grew the heap by {grew} bytes"
    );

    // Not vacuous: the appended prose was actually narrated.
    let spoken = fs::read_to_string(&speech_log).unwrap_or_default();
    for round in 0..6 {
        assert!(
            spoken.contains(&format!("Round {round} finished")),
            "round {round} was not narrated; speech log:\n{spoken}"
        );
    }
    assert!(
        !spoken.contains("Historical step"),
        "history was replayed:\n{spoken}"
    );
}
