//! Concurrent writers must not lose dedup state.
//!
//! Hand-ported from `engine/tests/test_spoken_lock.py`, whose docstring is the
//! whole argument:
//!
//! > Two hooks firing in parallel (CC + Codex, or two parallel CC sessions
//! > hitting the same session-id-mapped file via collision) would otherwise
//! > race on the read-modify-write of the per-session hash file, and one
//! > writer's new hash would silently overwrite the other's.
//!
//! Behavioural tests are hand-ported — there
//! is no shortcut for sockets, threads and files, and this is one of the few. A
//! fixture cannot express it: the bug is a lost update, and a corpus records
//! results, not interleavings.
//!
//! The Python test uses a `threading.Barrier` so every worker starts at the
//! same instant — "without the barrier most threads finish before any
//! contention happens and the test can pass even if the lock is missing." That
//! trick is load-bearing, so it is reproduced rather than approximated.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::thread;

use heard_state::spoken::SpokenStore;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("heard-state-lock-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn concurrent_mark_spoken_keeps_every_hash() {
    let dir = scratch("marks");
    let store = Arc::new(SpokenStore::new(&dir));
    let session = "session-x";

    const THREADS: usize = 16;
    const PER_THREAD: usize = 25;
    let total = THREADS * PER_THREAD;

    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = Vec::with_capacity(THREADS);
    for t in 0..THREADS {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            // Wait until all threads are ready, then race together.
            barrier.wait();
            for i in 0..PER_THREAD {
                store.mark_spoken(session, &format!("thread-{t}-text-{i}"));
            }
        }));
    }
    for handle in handles {
        handle.join().expect("worker panicked");
    }

    // Every text must still read as spoken. Checking through the public API
    // keeps the assertion about the behaviour that matters: nothing gets
    // re-narrated.
    let mut missing = Vec::new();
    for t in 0..THREADS {
        for i in 0..PER_THREAD {
            let text = format!("thread-{t}-text-{i}");
            if !store.is_spoken(session, &text) {
                missing.push(text);
            }
        }
    }
    assert!(
        missing.is_empty(),
        "{} of {total} hashes were lost to a race: {:?}",
        missing.len(),
        &missing[..missing.len().min(5)]
    );

    // And the file holds exactly that many distinct hashes — a lost update
    // shows up here as a short list even if a later writer happened to re-add
    // the same text.
    let body = fs::read_to_string(store.state_path(session)).expect("state file");
    let value = serde_json::from_str::<serde_json::Value>(&body).expect("valid JSON");
    let hashes = value["hashes"].as_array().expect("hashes array");
    assert_eq!(hashes.len(), total, "race lost writes");
    let distinct: HashSet<&str> = hashes.iter().filter_map(|h| h.as_str()).collect();
    assert_eq!(distinct.len(), total, "duplicates present");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn two_threads_hammering_one_file_never_show_a_torn_read() {
    // The other half of the contract: a reader must never observe a
    // half-written file. `_save` writes the whole document in one call, and
    // `is_spoken` tolerates a parse failure by returning false — but "false"
    // here would mean a line gets narrated twice, so the test asserts the
    // stronger property that the file always parses and the seed never
    // disappears mid-write.
    let dir = scratch("torn");
    let store = Arc::new(SpokenStore::new(&dir));
    let session = "s1";
    store.mark_spoken(session, "seed");

    let barrier = Arc::new(Barrier::new(2));
    let writer = {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            for i in 0..400 {
                store.mark_spoken(session, &format!("write-{i}"));
            }
        })
    };
    let reader = {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        let path = store.state_path(session);
        thread::spawn(move || {
            barrier.wait();
            let mut torn = 0;
            for _ in 0..400 {
                if let Ok(body) = fs::read_to_string(&path) {
                    if serde_json::from_str::<serde_json::Value>(&body).is_err() {
                        torn += 1;
                    }
                }
                assert!(store.is_spoken(session, "seed"), "the seed hash vanished");
            }
            torn
        })
    };

    writer.join().expect("writer panicked");
    let torn = reader.join().expect("reader panicked");
    assert_eq!(torn, 0, "reader saw {torn} half-written files");

    let _ = fs::remove_dir_all(&dir);
}
