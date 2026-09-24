//! The hold/replay primitive (`Speech::hold` / `Speech::release`): cut,
//! defer, rescue and flush, against the RECORDING player. Ports of the
//! Python daemon's `voice_hold` / `voice_release` / `tour_hold` /
//! `tour_release` arms and `_flush_deferred_while_mic`. Nothing plays.

use std::sync::Arc;
use std::time::Duration;

use heard_daemon::speech::{Hold, LineOptions, Speech, Utterance};
use heard_daemon::EventBus;
use heard_speech::{Admission, Delivery, PlayOutcome, QueuedSpeech, RecordingPlayer, SpeechItem};
use heard_state::clock::ManualClock;
use heard_tts::{Audio, Encoded, Tts, TtsError};

struct EchoTts;
impl Tts for EchoTts {
    fn audio_ext(&self) -> &'static str {
        ".mp3"
    }
    fn max_native_speed(&self) -> f64 {
        1.2
    }
    fn is_configured(&self) -> bool {
        true
    }
    fn synth(&self, text: &str, _v: &str, _s: f64, _l: &str) -> Result<Audio, TtsError> {
        Ok(Audio::Encoded(Encoded {
            bytes: text.as_bytes().to_vec(),
            ext: ".mp3",
        }))
    }
}

const WAIT: Duration = Duration::from_secs(3);

struct Rig {
    q: QueuedSpeech,
    player: Arc<RecordingPlayer>,
    clock: Arc<ManualClock>,
    dir: std::path::PathBuf,
}
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig(tag: &str, bus: Option<EventBus>) -> Rig {
    let dir = std::env::temp_dir().join(format!("heard-hold-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let player = Arc::new(RecordingPlayer::new());
    let clock = Arc::new(ManualClock::new(1000.0));
    let mut b = QueuedSpeech::builder(
        Arc::new(EchoTts),
        player.clone(),
        tokio::runtime::Handle::current(),
    )
    .tmp_dir(&dir)
    .history(&dir)
    .clock(clock.clone())
    .hold_exempt_sessions(&["__asked__"]);
    if let Some(bus) = bus {
        b = b.events(bus);
    }
    Rig {
        q: b.build(),
        player,
        clock,
        dir,
    }
}

fn line(t: &str) -> SpeechItem {
    SpeechItem::new(t).session("A")
}

async fn started(r: &Rig, text: &str) {
    let p = r.player.clone();
    let t = text.to_owned();
    assert!(
        tokio::task::spawn_blocking(move || p.wait_started(&t, WAIT))
            .await
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voice_hold_cuts_drops_the_queue_and_defers_until_release() {
    let r = rig("user", None);
    r.player.hold_on("playing");
    for t in ["playing", "queued-1", "queued-2"] {
        r.q.start_speech(line(t));
    }
    started(&r, "playing").await;
    r.q.hold(Hold::User);
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.player.calls()[0].outcome, PlayOutcome::Cancelled);
    // `_cancel_only`: the queued lines behind it are gone, not rescued.
    assert!(r.q.deferred().is_empty());
    assert!(r.q.is_holding());
    assert_eq!(r.q.start_speech(line("while-1")), Admission::Deferred);
    assert_eq!(r.q.start_speech(line("while-2")), Admission::Deferred);
    r.q.release(Hold::User);
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.player.finished(), vec!["while-1", "while-2"]);
    assert_eq!(
        r.q.deliveries()[0],
        ("playing".to_string(), Delivery::Cancelled)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tour_hold_rescues_the_queue_in_front_of_later_lines() {
    let r = rig("tour", None);
    r.player.hold_on("playing");
    for t in ["playing", "queued-1", "queued-2"] {
        r.q.start_speech(line(t));
    }
    started(&r, "playing").await;
    r.q.hold(Hold::Tour);
    assert!(r.q.wait_idle(WAIT).await);
    r.q.start_speech(line("during-tour"));
    let held: Vec<String> = r.q.deferred().into_iter().map(|d| d.0).collect();
    assert_eq!(held, vec!["queued-1", "queued-2", "during-tour"]);
    // A line the user asked for directly is not held by the tour.
    assert_eq!(
        r.q.start_speech(SpeechItem::new("your answer").session("__asked__")),
        Admission::Queued
    );
    assert!(r.q.wait_idle(WAIT).await);
    r.q.release(Hold::Tour);
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(
        r.player.finished(),
        vec!["your answer", "queued-1", "queued-2", "during-tour"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_waits_until_every_hold_is_released() {
    let r = rig("both", None);
    r.q.hold(Hold::Tour);
    r.q.hold(Hold::User);
    r.q.start_speech(line("held"));
    r.q.release(Hold::User);
    assert_eq!(r.q.deferred().len(), 1, "the tour still holds the floor");
    r.q.set_mic_active(true);
    r.q.release(Hold::Tour);
    assert_eq!(r.q.deferred().len(), 1, "the mic latch still holds it");
    r.q.clear_mic_latch();
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.player.finished(), vec!["held"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tour_hold_lapses_after_thirty_minutes() {
    let r = rig("lapse", None);
    r.q.hold(Hold::Tour);
    assert!(r.q.is_holding());
    r.clock.advance(1801.0);
    assert!(!r.q.is_holding());
    assert_eq!(r.q.start_speech(line("after")), Admission::Queued);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discard_held_forgets_the_backlog() {
    let r = rig("discard", None);
    r.q.hold(Hold::User);
    r.q.start_speech(line("stale"));
    r.q.discard_held();
    r.q.release(Hold::User);
    assert!(r.q.wait_idle(WAIT).await);
    assert!(r.player.finished().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speech_lifecycle_reaches_the_bus_and_ids_are_kept() {
    let bus = EventBus::new();
    let rx = bus.subscribe();
    let r = rig("events", Some(bus));
    let u = Utterance {
        text: "Done.",
        tag: "final_short",
        kind: "final",
        session_id: "A",
        via: "floor",
        project: "",
    };
    r.q.speak(&u);
    assert!(r.q.wait_idle(WAIT).await);
    let a = rx.recv_timeout(WAIT).unwrap();
    let b = rx.recv_timeout(WAIT).unwrap();
    assert_eq!(a, "{\"ev\": \"speech_started\", \"kind\": \"final\"}\n");
    assert_eq!(b, "{\"ev\": \"speech_finished\", \"kind\": \"final\"}\n");
    assert!(r.q.last_utterance_id().is_some());
    // No history meta → no history record, no id change.
    let id = r.q.last_utterance_id();
    r.q.speak_with(
        &Utterance {
            text: "On it.",
            via: "filler",
            kind: "",
            tag: "",
            ..u
        },
        LineOptions {
            priority: false,
            coexists: true,
            history: false,
        },
    );
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.q.last_utterance_id(), id);
    assert_eq!(r.q.queue_state(), (false, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_session_flushes_only_that_sessions_queued_lines() {
    let r = rig("dropsess", None);
    r.player.hold_on("playing");
    r.q.start_speech(line("playing"));
    started(&r, "playing").await;
    r.q.start_speech(line("a-1"));
    r.q.start_speech(line("a-2"));
    assert_eq!(r.q.drop_session("A"), 2);
    r.player.release();
    assert!(r.q.wait_idle(WAIT).await);
    assert_eq!(r.player.finished(), vec!["playing"]);
}
