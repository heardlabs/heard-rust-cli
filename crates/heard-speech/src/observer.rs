//! [`SpeechObserver`] — how something outside the queue learns what became
//! of each line, without the queue knowing what it is.
//!
//! The queue calls every observer, in the order they were added
//! ([`crate::queue::QueuedSpeechBuilder::observer`]):
//!
//! * [`SpeechObserver::on_spoken`] once per line the worker took off the
//!   queue, with its [`Delivery`] — played, cancelled, skipped or failed —
//!   right after the `history.jsonl` decision;
//! * [`SpeechObserver::on_expired`] once per line that was held while the
//!   listener was talking and aged out before it could be spoken. Such a
//!   line is never spoken; this is the only place it surfaces.
//!
//! The default is no observer at all. Observers run on the queue's worker
//! and under no lock; they must be quick and must not panic.

use crate::queue::{Delivery, SpeechItem};

/// Told what became of each line. Every method has a no-op default.
pub trait SpeechObserver: Send + Sync {
    /// A line the worker finished with, however it ended.
    fn on_spoken(&self, item: &SpeechItem, delivery: Delivery) {
        let _ = (item, delivery);
    }

    /// A held line that aged out unspoken (`_flush_deferred_while_mic`'s
    /// expired batch). An observer that keeps a record of the session would
    /// file it as `kind="unspoken"`, `tag="held_expired"`.
    fn on_expired(&self, item: &SpeechItem) {
        let _ = item;
    }
}
