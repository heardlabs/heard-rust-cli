//! heard-daemon — the daemon that ties the core crates together.
//!
//! ```text
//!            heard-proto            heard-config
//!        (the wire, as types)      (config.yaml, paths)
//!                 │                        │
//!   socket ──▶ dispatch ──▶ hook lane ──▶ routing ──▶ Speech sink
//!                             │              │            │
//!                      heard-narrate   heard-state    LogSpeech
//!                   (templates, verbosity, markdown)  (JSONL, history)
//!                                            │
//!                                     Brain (not ported) ──▶ floor
//! ```
//!
//! # What this crate does and does not do
//!
//! It accepts the whole socket protocol, does the non-narration commands for
//! real, and routes narration through the ported crates up to the point where
//! it WOULD speak — then records instead of speaking. **There is no audio path
//! in this crate at all**: no TTS, no `afplay`, no `say`. The
//! [`speech::Speech`] trait is where `heard-speech` attaches one.
//!
//! That seam is what makes **differential running** possible: the
//! Rust daemon runs beside the Python reference implementation on a DIFFERENT socket, hook traffic
//! is teed to both (`heard-hook` with `HEARD_DIFFERENTIAL_SOCKET`), and what
//! each would have said is diffed.
//!
//! # The binary lives in a composition root
//!
//! `heard-speech` (and any brain) implement this crate's [`Brain`] and
//! [`Speech`] seams and so DEPEND on it; a binary here could not link them
//! without a dependency cycle. The executable is therefore built by a
//! composition root that picks the real implementations by flag and config.
//! This crate stays the library: routing, the socket protocol, and the seams
//! ([`Brain`], [`Speech`], [`Extension`], [`Capture`]).
//!
//! # Extensions
//!
//! Features that are not part of the core attach through [`Extension`]: the
//! daemon holds an ordered list of them (set with
//! [`DaemonBuilder::extension`]) and hands them every socket command it does
//! not know, every narration event, every spoken line, and the chance to
//! mark kinds as verbatim. See [`extension`].
//!
//! # The socket
//!
//! [`heard_proto::transport::socket_path`] resolves `HEARD_DAEMON_SOCKET`
//! first, and this binary requires it: binding the real
//! `~/Library/Application Support/heard/daemon.sock` would take it from a
//! live Python daemon, and two daemons on one socket is a coin flip over
//! which one gets each hook. See [`bin::resolve_socket`].
//!
//! # Concurrency
//!
//! One `tokio` runtime and a handful of tasks, not a thread per subsystem:
//! an accept loop, one short-lived task per connection, and one task per
//! ACTIVE agent session in the hook lane ([`hooks::HookQueue`]) which retires
//! after two idle minutes.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod bin;
pub mod brain;
pub mod capture;
pub mod daemon;
pub mod dedup;
pub mod events;
pub mod extension;
pub mod filler;
pub mod floor;
pub mod hooks;
pub mod log;
pub mod policy;
pub mod server;
pub mod shape;
pub mod speech;
pub mod testing;
pub mod transcript;

pub use brain::{Brain, BrainDecision, BrainRequest, NoBrain};
pub use capture::{Capture, NoCapture};
pub use daemon::{
    BundledPersonas, Daemon, DaemonBuilder, Line, NarrationEvent, Outcome, PersonaInfo,
    PersonaSource,
};
pub use events::EventBus;
pub use extension::{Extension, SpokenLine};
pub use hooks::HookQueue;
pub use server::Server;
pub use speech::{
    Hold, LineOptions, LogSpeech, NullSpeech, Speech, Utterance, VIA_FILLER, VIA_NOTICE,
};
