//! heard-narrate — the pure narration path, ported from Python.
//!
//! Three modules, all pure functions over text
//! the process already owns:
//!
//! * [`markdown`] — strips markdown out of assistant prose before TTS.
//! * [`templates`] — the per-tool narration templates: what Heard says when an
//!   agent runs a command, edits a file, asks a question or fails.
//! * [`verbosity`] — the three-way classifier that decides whether an event is
//!   spoken, dropped, or accumulated into a digest, plus the [`profile`] it
//!   reads that decision from.
//!
//! ## Parity
//!
//! Every function here is proven against the Python it replaces by the golden
//! corpus in `fixtures/narrate/` — `(input, output)` records generated from
//! the Python tests' own literals by the fixture generator (not included),
//! asserted on the Python side by the reference implementation and on this
//! side by `tests/corpus.rs`. A fixture pins CURRENT behaviour; it is
//! not a judgement that the behaviour is right.
//!
//! ## Allocation
//!
//! The pipeline is `&str` and [`std::borrow::Cow`] end to end. Text that needs
//! no change is handed back borrowed — a markdown-free assistant line, a file
//! path's basename, a `ctx` value lifted straight out of the tool payload — and
//! only the strings that are genuinely built (`"Editing auth."`) allocate.

pub mod markdown;
pub mod profile;
pub mod templates;
pub mod verbosity;

pub use markdown::strip;
pub use profile::Profile;
pub use templates::{post_tool_event, pre_tool_event, Narration};
pub use verbosity::{classify_post, classify_pre, classify_prose, Cfg, Decision};
