//! Behavioural tests for the speech queue, hand-ported from
//! `engine/tests/test_speech_queue.py` and `test_muted.py`, against the
//! RECORDING player. Nothing here makes a sound: the TTS writes the words
//! themselves into the "audio" file and the player reads them back.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use heard_daemon::speech::{Speech, Utterance};
use heard_speech::{
    Admission, Delivery, PlayOutcome, QueuedSpeech, RecordingPlayer, SpeechItem, SpeechLimits,
    SpeechObserver, SpeechSettings,
};
use heard_state::clock::{Clock, ManualClock};
use heard_tts::{Audio, Encoded, Tts, TtsError};

#[derive(Default)]
struct FakeTts {
    max_native: f64,
    unconfigured: bool,
    hold: AtomicBool,
    gate: (Mutex<()>, Condvar),
    synth_started: AtomicUsize,
    fail_on: Mutex<Vec<String>>,
    seen: Mutex<Vec<(String, String, f64)>>,
}

impl FakeTts {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            max_native: 1.2,
            ..Self::default()
        })
    }
    fn release(&self) {
        self.hold.store(false, Ordering::SeqCst);
        self.gate.1.notify_all();
    }
}

impl Tts for FakeTts {
    fn audio_ext(&self) -> &'static str {
        ".mp3"
    }
    fn max_native_speed(&self) -> f64 {
        self.max_native
    }
    fn is_configured(&self) -> bool {
        !self.unconfigured
    }
    fn synth(&self, text: &str, voice: &str, speed: f64, _lang: &str) -> Result<Audio, TtsError> {
        self.synth_started.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap()
            .push((text.into(), voice.into(), speed));
        let mut g = self.gate.0.lock().unwrap();
        while self.hold.load(Ordering::SeqCst) {
            g = self
                .gate
                .1
                .wait_timeout(g, Duration::from_millis(5))
                .unwrap()
                .0;
        }
        drop(g);
        if self.fail_on.lock().unwrap().iter().any(|f| f == text) {
            return Err(TtsError::Io(std::io::Error::other("fake synth failure")));
        }
        Ok(Audio::Encoded(Encoded {
            bytes: text.as_bytes().to_vec(),
            ext: ".mp3",
        }))
    }
}

struct Rig {
    q: QueuedSpeech,
    tts: Arc<FakeTts>,
    player: Arc<RecordingPlayer>,
    dir: std::path::PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn tmp(tag: &str) -> std::path::PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let p = std::env::temp_dir().join(format!(
        "heard-speech-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn rig_with(
    tag: &str,
    tts: Arc<FakeTts>,
    player: Arc<RecordingPlayer>,
    f: impl FnOnce(heard_speech::queue::QueuedSpeechBuilder) -> heard_speech::queue::QueuedSpeechBuilder,
) -> Rig {
    let dir = tmp(tag);
    let b = QueuedSpeech::builder(
        tts.clone(),
        player.clone(),
        tokio::runtime::Handle::current(),
    )
    .tmp_dir(dir.join("audio"))
    .settings(SpeechSettings {
        voice: "Rachel".into(),
        persona: "jarvis".into(),
        ..SpeechSettings::default()
    });
    std::fs::create_dir_all(dir.join("audio")).unwrap();
    Rig {
        q: f(b).build(),
        tts,
        player,
        dir,
    }
}

fn rig(tag: &str) -> Rig {
    rig_with(tag, FakeTts::new(), Arc::new(RecordingPlayer::new()), |b| b)
}

const WAIT: Duration = Duration::from_secs(3);

fn line(t: &str) -> SpeechItem {
    SpeechItem::new(t)
}

async fn started(r: &Rig, text: &str) {
    let player = r.player.clone();
    let text = text.to_owned();
    let ok = tokio::task::spawn_blocking(move || player.wait_started(&text, WAIT))
        .await
        .unwrap();
    assert!(ok, "player never started the line");
}

// --- ordering --------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_serialises_utterances() {
    let r = rig_with(
        "serial",
        FakeTts::new(),
        Arc::new(RecordingPlayer::with_duration(Duration::from_millis(20))),
        |b| b,
    );
    for t in ["first", "second", "third"] {
        assert_eq!(r.q.start_speech(line(t)), Admission::Queued);
    }
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.player.finished(), vec!["first", "second", "third"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_caps_at_max_and_drops_the_oldest() {
    let r = rig("cap");
    r.player.hold_on("first");
    r.q.start_speech(line("first"));
    started(&r, "first").await;
    for i in 0..5 {
        r.q.start_speech(line(&format!("q{i}")));
    }
    r.q.start_speech(line("overflow"));
    assert_eq!(r.q.queued(), vec!["q1", "q2", "q3", "q4", "overflow"]);
    r.player.release();
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(
        r.player.finished(),
        vec!["first", "q1", "q2", "q3", "q4", "overflow"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn priority_ack_jumps_to_the_front() {
    let r = rig("priority");
    r.player.hold_on("inflight");
    r.q.start_speech(line("inflight"));
    started(&r, "inflight").await;
    r.q.start_speech(line("narration-1"));
    r.q.start_speech(line("narration-2"));
    r.q.start_speech(line("on it — checking now").priority(true));
    assert_eq!(
        r.q.queued(),
        vec!["on it — checking now", "narration-1", "narration-2"]
    );
    r.player.release();
    assert!(r.q.wait_idle(WAIT).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_final_supersedes_its_own_routine_lines_but_never_jumps_them() {
    let r = rig("final");
    r.player.hold_on("pin");
    r.q.start_speech(line("pin").session("A"));
    started(&r, "pin").await;
    r.q.start_speech(line("reading auth").session("A").kind("tool_pre"));
    r.q.start_speech(line("which one?").session("A").kind("question"));
    r.q.start_speech(line("still going").session("A").kind("intermediate"));
    r.q.start_speech(line("Done.").session("A").kind("final").priority(true));
    assert_eq!(r.q.queued(), vec!["which one?", "Done."]);
    r.player.release();
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.player.finished(), vec!["pin", "which one?", "Done."]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_session_drops_other_sessions_queued_lines() {
    let r = rig("sessions");
    r.player.hold_on("session-A-first");
    for t in ["session-A-first", "session-A-second", "session-A-third"] {
        r.q.start_speech(line(t).session("A"));
    }
    started(&r, "session-A-first").await;
    r.q.start_speech(line("session-B-first").session("B"));
    r.player.release();
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(
        r.player.finished(),
        vec!["session-A-first", "session-B-first"]
    );
}

// --- cancellation ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silence_cancels_the_line_in_flight_and_clears_the_queue() {
    let r = rig("silence");
    r.player.hold_on("playing-now");
    for t in ["playing-now", "queued-1", "queued-2"] {
        r.q.start_speech(line(t));
    }
    started(&r, "playing-now").await;
    r.q.cancel_only();
    assert!(r.q.wait_idle(WAIT).await);
    assert!(r.player.finished().is_empty());
    let calls = r.player.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].outcome, PlayOutcome::Cancelled);
    assert_eq!(
        r.q.deliveries(),
        vec![("playing-now".to_string(), Delivery::Cancelled)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silence_interrupts_an_in_flight_synth_at_once() {
    let tts = FakeTts::new();
    tts.hold.store(true, Ordering::SeqCst);
    let r = rig_with("synth-cancel", tts, Arc::new(RecordingPlayer::new()), |b| b);
    r.q.start_speech(line("hello world"));
    let deadline = Instant::now() + WAIT;
    while r.tts.synth_started.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(
        r.tts.synth_started.load(Ordering::SeqCst),
        1,
        "synth never started"
    );
    let t0 = Instant::now();
    r.q.cancel_only();
    assert!(
        r.q.wait_idle(Duration::from_millis(500)).await,
        "worker did not return after cancel"
    );
    assert!(t0.elapsed() < Duration::from_millis(500));
    // The orphaned synth finishes on its own and must not reach the player.
    r.tts.release();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        r.player.calls().is_empty(),
        "a cancelled synth reached playback"
    );
    let leftovers = std::fs::read_dir(r.dir.join("audio")).unwrap().count();
    assert_eq!(
        leftovers, 0,
        "the abandoned synth left its temp file behind"
    );
}

// --- mic deferral ----------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mic_active_defers_then_flushes_on_release() {
    let r = rig("mic");
    r.q.set_mic_active(true);
    assert_eq!(
        r.q.start_speech(line("held-1").session("A")),
        Admission::Deferred
    );
    assert_eq!(
        r.q.start_speech(line("held-2").session("A")),
        Admission::Deferred
    );
    assert!(r.q.queued().is_empty());
    assert_eq!(r.q.deferred().len(), 2);
    r.q.set_mic_active(false);
    r.q.flush_deferred();
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.player.finished(), vec!["held-1", "held-2"]);
    assert!(r.q.deferred().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_held_buffer_keeps_progress_and_results() {
    let r = rig("held-both");
    r.q.set_mic_active(true);
    r.q.start_speech(line("still working on it").kind("intermediate"));
    r.q.start_speech(line("done — network's built").kind("final").priority(true));
    assert_eq!(
        r.q.deferred(),
        vec![
            ("still working on it".into(), false),
            ("done — network's built".into(), true)
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_held_buffer_caps_at_ten_dropping_routine_first() {
    let r = rig("held-cap");
    r.q.set_mic_active(true);
    r.q.start_speech(line("result").kind("final").priority(true));
    for i in 0..13 {
        r.q.start_speech(line(&format!("line-{i}")).kind("intermediate"));
    }
    let held: Vec<String> = r.q.deferred().into_iter().map(|d| d.0).collect();
    assert_eq!(held.len(), 10);
    assert_eq!(
        held[0], "result",
        "a held result survives a chatty dictation"
    );
    assert_eq!(held[1], "line-4");
    assert_eq!(held[9], "line-12");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_flush_waits_out_the_mic_grace() {
    let r = rig("grace");
    r.q.set_mic_active(true);
    r.q.set_user_speaking(true);
    r.q.start_speech(line("held-1").session("A"));
    r.q.start_speech(line("held-2").session("A"));
    r.q.set_user_speaking(false);
    r.q.flush_deferred();
    assert!(r.q.queued().is_empty());
    assert_eq!(r.q.deferred().len(), 2, "channel A still holds the floor");
    r.q.set_mic_active(false);
    r.q.flush_deferred();
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.player.finished(), vec!["held-1", "held-2"]);
}

/// Records every observer callback, in order.
#[derive(Default)]
struct Watch {
    seen: Mutex<Vec<(String, String, String)>>,
}

impl Watch {
    fn seen(&self) -> Vec<(String, String, String)> {
        self.seen.lock().unwrap().clone()
    }
}

impl SpeechObserver for Watch {
    fn on_spoken(&self, item: &SpeechItem, delivery: Delivery) {
        self.seen.lock().unwrap().push((
            "spoken".into(),
            item.text.clone(),
            format!("{delivery:?}"),
        ));
    }
    fn on_expired(&self, item: &SpeechItem) {
        self.seen.lock().unwrap().push((
            "expired".into(),
            item.text.clone(),
            item.session_id.clone(),
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_lines_expire_into_the_observers() {
    let clock = Arc::new(ManualClock::new(10_000.0));
    let watch = Arc::new(Watch::default());
    let (c2, w2) = (clock.clone(), watch.clone());
    let r = rig_with(
        "expire",
        FakeTts::new(),
        Arc::new(RecordingPlayer::new()),
        move |b| b.clock(c2).observer(w2),
    );
    r.q.set_mic_active(true);
    r.q.start_speech(line("stale").session("A"));
    clock.advance(301.0);
    r.q.start_speech(line("fresh").session("A"));
    r.q.set_mic_active(false);
    r.q.flush_deferred();
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.player.finished(), vec!["fresh"]);
    assert_eq!(
        watch.seen(),
        vec![
            ("expired".to_string(), "stale".to_string(), "A".to_string()),
            (
                "spoken".to_string(),
                "fresh".to_string(),
                "Played".to_string()
            ),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_finished_line_reaches_every_observer_in_order() {
    let (a, b) = (Arc::new(Watch::default()), Arc::new(Watch::default()));
    let (a2, b2) = (a.clone(), b.clone());
    let r = rig_with(
        "observers",
        FakeTts::new(),
        Arc::new(RecordingPlayer::new()),
        move |q| q.observer(a2).observer(b2),
    );
    r.q.start_speech(line("one"));
    r.q.start_speech(line("two"));
    assert!(r.q.wait_idle(WAIT).await);
    let want = vec![
        (
            "spoken".to_string(),
            "one".to_string(),
            "Played".to_string(),
        ),
        (
            "spoken".to_string(),
            "two".to_string(),
            "Played".to_string(),
        ),
    ];
    assert_eq!(a.seen(), want);
    assert_eq!(b.seen(), want);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_mic_latch_is_cleared_and_narration_resumes() {
    let clock = Arc::new(ManualClock::new(10_000.0));
    let c2 = clock.clone();
    let r = rig_with(
        "ceiling",
        FakeTts::new(),
        Arc::new(RecordingPlayer::new()),
        move |b| b.clock(c2),
    );
    r.q.set_mic_active(true);
    clock.advance(90.0 + 30.0);
    assert_eq!(r.q.start_speech(line("resumed")), Admission::Queued);
    assert!(r.q.wait_idle(WAIT).await);
    assert!(!r.q.mic_active());
    assert_eq!(r.player.finished(), vec!["resumed"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_latch_still_defers_and_a_call_buys_the_long_backstop() {
    let clock = Arc::new(ManualClock::new(10_000.0));
    let c2 = clock.clone();
    let r = rig_with(
        "backstop",
        FakeTts::new(),
        Arc::new(RecordingPlayer::new()),
        move |b| b.clock(c2).call_app_running(|| true),
    );
    r.q.set_mic_active(true);
    assert_eq!(r.q.start_speech(line("held")), Admission::Deferred);
    clock.advance(90.0 + 60.0);
    assert_eq!(r.q.start_speech(line("during-call")), Admission::Deferred);
    assert!(r.q.mic_active());
    assert!(r.player.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn barge_in_rescues_unplayed_lines_and_the_grace_replays_them() {
    let r = rig_with(
        "barge",
        FakeTts::new(),
        Arc::new(RecordingPlayer::new()),
        |b| {
            b.limits(SpeechLimits {
                mic_release_grace: Duration::from_millis(40),
                ..SpeechLimits::default()
            })
        },
    );
    r.player.hold_on("speaking");
    r.q.start_speech(line("speaking").session("A"));
    started(&r, "speaking").await;
    r.q.start_speech(line("next-1").session("A"));
    r.q.start_speech(line("next-2").session("A"));
    r.q.on_mic_active();
    assert!(r.q.queued().is_empty());
    assert_eq!(
        r.q.deferred().into_iter().map(|d| d.0).collect::<Vec<_>>(),
        vec!["next-1", "next-2"]
    );
    // Released, then re-tripped inside the grace: still held.
    r.q.on_mic_released();
    r.q.on_mic_active();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(r.q.mic_active());
    assert_eq!(r.q.deferred().len(), 2);
    // A real release.
    r.q.on_mic_released();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.player.finished(), vec!["next-1", "next-2"]);
    assert_eq!(r.player.calls()[0].outcome, PlayOutcome::Cancelled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambient_mode_never_latches_the_mic() {
    let r = rig("ambient");
    r.q.set_settings(SpeechSettings {
        voice_mode: "ambient".into(),
        ..r.q.settings()
    });
    r.q.on_mic_active();
    assert!(!r.q.mic_active());
    assert_eq!(r.q.start_speech(line("narrated")), Admission::Queued);
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.player.finished(), vec!["narrated"]);
}

// --- mute ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mute_clears_the_held_buffer_and_drops_new_lines() {
    let r = rig("mute");
    r.q.set_mic_active(true);
    r.q.start_speech(line("held").kind("final").priority(true));
    assert!(!r.q.deferred().is_empty());
    r.q.mute();
    assert!(r.q.deferred().is_empty());
    r.q.set_mic_active(false);
    assert_eq!(
        r.q.start_speech(line("while muted")),
        Admission::Dropped("muted")
    );
    r.q.unmute();
    assert_eq!(r.q.start_speech(line("after")), Admission::Queued);
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.player.finished(), vec!["after"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_line_queued_before_mute_is_not_synthesised_after_it() {
    let r = rig("mute-late");
    r.player.hold_on("first");
    r.q.start_speech(line("first"));
    started(&r, "first").await;
    r.q.start_speech(line("second"));
    // Flip muted WITHOUT the queue clear, the belt-and-braces case `_speak`
    // re-checks for.
    r.q.set_settings(SpeechSettings {
        muted: true,
        ..r.q.settings()
    });
    r.player.release();
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.player.finished(), vec!["first"]);
    assert_eq!(
        r.q.deliveries()[1],
        ("second".to_string(), Delivery::Skipped("muted"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_off_and_empty_text_never_queue() {
    let r = rig("audio-off");
    assert_eq!(
        r.q.start_speech(line("   ")),
        Admission::Dropped("empty_text")
    );
    r.q.set_settings(SpeechSettings {
        audio_off: true,
        ..r.q.settings()
    });
    assert_eq!(r.q.start_speech(line("x")), Admission::Dropped("audio_off"));
    assert!(r.player.calls().is_empty());
}

// --- synthesis and playback ------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speed_above_native_is_layered_with_afplay_rate() {
    let r = rig("speed");
    r.q.set_settings(SpeechSettings {
        speed: 1.8,
        ..r.q.settings()
    });
    r.q.start_speech(line("fast"));
    assert!(r.q.wait_idle(WAIT).await);
    r.q.set_settings(SpeechSettings {
        speed: 1.0,
        ..r.q.settings()
    });
    r.q.start_speech(line("normal"));
    assert!(r.q.wait_idle(WAIT).await);
    let calls = r.player.calls();
    assert!((calls[0].rate - 1.5).abs() < 1e-9, "{}", calls[0].rate);
    assert_eq!(calls[1].rate, 1.0);
    assert_eq!(calls[0].ext, ".mp3");
    // The backend is always asked for the requested speed.
    let seen = r.tts.seen.lock().unwrap().clone();
    assert_eq!(seen[0].2, 1.8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_text_is_chunked_and_every_temp_file_is_removed() {
    let r = rig("chunks");
    let text = format!("{}. {}.", "a".repeat(700), "b".repeat(700));
    r.q.start_speech(line(&text));
    assert!(r.q.wait_idle(WAIT).await);
    let played = r.player.finished();
    assert_eq!(
        played,
        vec![
            format!("{}.", "a".repeat(700)),
            format!("{}.", "b".repeat(700))
        ]
    );
    assert_eq!(std::fs::read_dir(r.dir.join("audio")).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_voice_configured_skips_without_touching_the_player() {
    let tts = Arc::new(FakeTts {
        max_native: 1.2,
        unconfigured: true,
        ..FakeTts::default()
    });
    let r = rig_with("novoice", tts, Arc::new(RecordingPlayer::new()), |b| b);
    r.q.start_speech(line("hello"));
    assert!(r.q.wait_idle(WAIT).await);
    assert!(r.player.calls().is_empty());
    assert_eq!(
        r.q.deliveries(),
        vec![(
            "hello".to_string(),
            Delivery::Skipped("no_voice_configured")
        )]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kokoro_voices_come_from_their_own_namespace() {
    let r = rig("kokoro-voice");
    r.q.set_settings(SpeechSettings {
        use_kokoro_voice: true,
        kokoro_voice: "af_heart".into(),
        ..r.q.settings()
    });
    r.q.start_speech(line("one"));
    let mut overridden = line("two");
    overridden.voice_override = Some("am_adam".into());
    r.q.start_speech(overridden);
    assert!(r.q.wait_idle(WAIT).await);
    let seen = r.tts.seen.lock().unwrap().clone();
    assert_eq!(seen[0].1, "af_heart");
    assert_eq!(seen[1].1, "am_adam");
}

// --- history ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_records_every_line_that_was_not_cancelled() {
    // Python's `_drain_queue`: `if not cancel.is_set(): if hmeta:
    // history.append(...)` — played, synth-failed, afplay-failed and
    // skipped lines are all recorded; only a cancelled line (silence,
    // barge-in) and a line with no history meta are not.
    let dir = tmp("history-cfg");
    let d2 = dir.clone();
    let r = rig_with(
        "history",
        FakeTts::new(),
        Arc::new(RecordingPlayer::new()),
        move |b| b.history(d2),
    );
    r.tts.fail_on.lock().unwrap().push("broken".into());
    r.player.fail_on("afplay-fails");
    let meta = |t: &str| line(t).session("s1").kind("tool_pre");
    let mut played = meta("Editing auth.");
    played.via = "fastpath".into();
    played.project = "heard".into();
    played.tag = "tool_edit".into();
    r.q.start_speech(played);
    r.q.start_speech(meta("broken"));
    r.q.start_speech(meta("afplay-fails"));
    r.q.start_speech(line("bare line, no meta").session("s1"));
    assert!(r.q.wait_idle(WAIT).await);
    // Skipped by `_speak` because the mic went hot after it was queued.
    r.q.set_mic_active(true);
    r.q.enqueue(meta("skipped: mic active"));
    assert!(r.q.wait_idle(WAIT).await);
    r.q.set_mic_active(false);
    // Skipped by `_speak`'s belt-and-braces mute re-check.
    r.q.set_settings(SpeechSettings {
        muted: true,
        ..r.q.settings()
    });
    r.q.enqueue(meta("skipped: muted"));
    assert!(r.q.wait_idle(WAIT).await);
    r.q.set_settings(SpeechSettings {
        muted: false,
        ..r.q.settings()
    });
    // Cancelled mid-play by silence, then by a barge-in.
    r.player.hold_on("cut off");
    r.q.start_speech(meta("cut off"));
    started(&r, "cut off").await;
    r.q.cancel_only();
    assert!(r.q.wait_idle(WAIT).await);
    r.player.hold_on("barged");
    r.q.start_speech(meta("barged"));
    started(&r, "barged").await;
    r.q.on_mic_active();
    assert!(r.q.wait_idle(WAIT).await);

    let body = std::fs::read_to_string(dir.join("history.jsonl")).expect("history written");
    let lines: Vec<&str> = body.lines().collect();
    let spoken: Vec<String> = lines
        .iter()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["spoken"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        spoken,
        vec![
            "Editing auth.",
            "broken",
            "afplay-fails",
            "skipped: mic active",
            "skipped: muted"
        ],
        "{body}"
    );
    assert_eq!(
        r.player.finished(),
        vec!["Editing auth.", "bare line, no meta"],
        "the recorded failures and skips never played"
    );
    let rec: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(rec["spoken"], "Editing auth.");
    assert_eq!(rec["kind"], "tool_pre");
    assert_eq!(rec["tag"], "tool_edit");
    assert_eq!(rec["via"], "fastpath");
    assert_eq!(rec["repo_name"], "heard");
    assert_eq!(rec["session_id"], "s1");
    assert_eq!(rec["voice"], "Rachel");
    assert_eq!(rec["persona"], "jarvis");
    assert!(rec["id"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(
        lines[0].starts_with("{\"kind\": \"tool_pre\""),
        "Python spacing: {}",
        lines[0]
    );
    let _ = std::fs::remove_dir_all(dir);
}

// --- the Speech sink -------------------------------------------------------

fn utterance<'a>(text: &'a str, kind: &'a str, via: &'a str, session: &'a str) -> Utterance<'a> {
    Utterance {
        text,
        tag: "tool_bash",
        kind,
        session_id: session,
        via,
        project: "heard",
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sink_suppresses_a_repeated_tool_line_and_prioritises_finals() {
    let r = rig("sink");
    r.player.hold_on("pin");
    r.q.start_speech(line("pin").session("s1"));
    started(&r, "pin").await;
    let sink: &dyn Speech = &r.q;
    sink.speak(&utterance("Running tests.", "tool_pre", "fastpath", "s1"));
    sink.speak(&utterance("running   TESTS.", "tool_pre", "fastpath", "s1"));
    sink.speak(&utterance(
        "Running tests.",
        "tool_pre",
        "fastpath",
        "s2-other",
    ));
    assert_eq!(
        r.q.queued(),
        vec!["Running tests."],
        "repeat dropped; s2 cleared s1's line"
    );
    sink.speak(&utterance(
        "Checked the build.",
        "intermediate",
        "brain",
        "s2-other",
    ));
    sink.speak(&utterance("All green.", "final", "brain", "s2-other"));
    assert_eq!(
        r.q.queued(),
        vec!["All green."],
        "the final supersedes its session's routine lines"
    );
    r.player.release();
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.player.finished(), vec!["pin", "All green."]);
}

#[test]
fn nothing_in_the_test_suite_can_reach_afplay() {
    // A guard, not a behaviour: the recording player is the only player the
    // tests construct. This keeps a future test from swapping in the real one
    // by accident without someone reading this line.
    let src = include_str!("queue.rs");
    let needle = ["Afplay", "Player::new"].concat();
    assert!(!src.contains(&needle));
    let _ = ManualClock::new(0.0).monotonic();
}
