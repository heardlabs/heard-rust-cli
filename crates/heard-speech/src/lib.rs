//! heard-speech — the speech queue.
//!
//! [`QueuedSpeech`] is a [`heard_daemon::Speech`] sink, so the daemon can be
//! pointed at it instead of `LogSpeech` without changing a line of routing:
//!
//! ```text
//!  routing ──▶ Speech::speak ──▶ QueuedSpeech ──▶ Tts::synth ──▶ Player::play
//!                                  │  (queue, priority, cancel,    (afplay)
//!                                  │   mic deferral, mute)
//!                                  ├──▶ history.jsonl (every line not cancelled)
//!                                  └──▶ SpeechObserver (every line, and the
//!                                        held lines that expired unspoken)
//! ```
//!
//! | module | Python |
//! |---|---|
//! | [`policy`] | the pure list surgery in `_enqueue_speech`, `_flush_deferred_while_mic`, `_start_speech`'s held-buffer cap, `_split`, and the `afplay -r` rate |
//! | [`queue`] | `_start_speech`, `_enqueue_speech`, `_drain_queue`, `_speak`, `_cancel_only`, `_on_mic_active`, `_on_mic_released`, `_do_mute` |
//! | [`player`] | the `afplay` `Popen` + kill, as a trait with a recording test impl |
//! | [`observer`] | the seam an outside record of the session (memory, analytics) attaches to |
//!
//! **Nothing here plays audio in a test.** [`player::AfplayPlayer`] is the
//! real player; it is compiled, and every test drives
//! [`player::RecordingPlayer`] instead. The daemon's default sink is still
//! `NullSpeech` / `LogSpeech`; wiring this in is a one-line builder change
//! left to the composition root.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod observer;
pub mod player;
pub mod policy;
pub mod queue;

pub use observer::SpeechObserver;
pub use player::{AfplayPlayer, Cancel, PlayOutcome, Player, RecordingPlayer};
pub use queue::{
    Admission, Delivery, QueuedSpeech, SettingsSource, SpeechItem, SpeechLimits, SpeechSettings,
};
