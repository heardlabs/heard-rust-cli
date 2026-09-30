//! Classifying a frame: the core's closed set, or an extension command.
//!
//! [`crate::Message`] is the daemon's reading of a frame: a recognised `cmd`
//! is that command, and anything else falls through to `speak`. That is
//! still true. What this module adds is the one fact the fall-through throws
//! away: that the frame DID carry a `cmd`, just not one the core knows.
//!
//! [`parse_frame`] keeps that fact. A frame whose `cmd` is not in
//! [`CORE_CMDS`] comes back as [`Frame::Extension`], carrying the command
//! name, the frame's **raw bytes** (so an extension parses exactly what the
//! client sent), and the `speak` fall-through the core applies when no
//! extension claims it. Every other frame comes back as [`Frame::Core`] with
//! the same [`crate::Message`] a plain `serde_json::from_slice` gives.

use std::borrow::Cow;

use serde::Deserialize;

use crate::request::{Message, Speak};

/// Every `cmd` the core's closed set handles, including the implicit
/// `speak`. A `cmd` outside this list is an extension command.
///
/// `cancel`, `voice_hold` / `voice_release`, `tour_hold` / `tour_release`,
/// `subscribe` and `inject` are core commands the daemon handles without a
/// typed [`crate::Request`] variant; they are listed so nothing else can
/// claim them.
pub const CORE_CMDS: &[&str] = &[
    "speak",
    "ping",
    "status",
    "pin",
    "unpin",
    "reload",
    "stop",
    "mute",
    "unmute",
    "resume_intent",
    "feedback",
    "report_defect",
    "mute_session",
    "unmute_session",
    "event",
    "hook",
    "cancel",
    "voice_hold",
    "voice_release",
    "tour_hold",
    "tour_release",
    "subscribe",
    "inject",
];

/// Is `cmd` one of the core's own commands?
#[must_use]
pub fn is_core_cmd(cmd: &str) -> bool {
    CORE_CMDS.contains(&cmd)
}

/// A frame whose `cmd` the core does not know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionCommand<'a> {
    /// The frame's `cmd`, unescaped.
    pub cmd: Cow<'a, str>,
    /// The whole frame, byte for byte as it arrived.
    pub raw: &'a [u8],
    /// What the core does with the frame when no extension claims it: the
    /// `speak` fall-through (`req.get("text") or ""`).
    pub fallback: Speak<'a>,
}

/// A frame, classified.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame<'a> {
    /// A frame the core's closed set handles: a known command, or the
    /// `speak` fall-through of a frame with no `cmd` (or a non-string one,
    /// or a known `cmd` whose fields did not parse).
    Core(Message<'a>),
    /// A frame whose string `cmd` is not in [`CORE_CMDS`].
    Extension(ExtensionCommand<'a>),
}

#[derive(Deserialize)]
struct Peek<'a> {
    #[serde(borrow, default)]
    cmd: Option<Cow<'a, str>>,
}

/// Parse and classify one frame.
///
/// # Errors
///
/// Exactly when `serde_json::from_slice::<Message>` fails: the payload is
/// not JSON, or not an object the `speak` fall-through can read.
pub fn parse_frame(raw: &[u8]) -> Result<Frame<'_>, serde_json::Error> {
    let message: Message<'_> = serde_json::from_slice(raw)?;
    let speak = match message {
        Message::Command(_) => return Ok(Frame::Core(message)),
        Message::Speak(speak) => speak,
    };
    // Only a frame that mentions `"cmd"` at all pays for the second parse.
    if !raw.windows(5).any(|w| w == b"\"cmd\"") {
        return Ok(Frame::Core(Message::Speak(speak)));
    }
    match serde_json::from_slice::<Peek<'_>>(raw) {
        Ok(Peek { cmd: Some(cmd) }) if !is_core_cmd(&cmd) => {
            Ok(Frame::Extension(ExtensionCommand {
                cmd,
                raw,
                fallback: speak,
            }))
        }
        _ => Ok(Frame::Core(Message::Speak(speak))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Request;

    #[test]
    fn a_known_command_is_core() {
        let f = parse_frame(br#"{"cmd":"ping"}"#).unwrap();
        assert_eq!(f, Frame::Core(Message::Command(Request::Ping)));
    }

    #[test]
    fn a_cmdless_frame_is_the_core_speak() {
        let f = parse_frame(br#"{"text":"hi"}"#).unwrap();
        assert_eq!(
            f,
            Frame::Core(Message::Speak(Speak {
                text: "hi".into(),
                priority: false
            }))
        );
    }

    #[test]
    fn an_unknown_command_keeps_its_raw_bytes_and_its_fallback() {
        let raw: &[u8] = br#"{"cmd": "ask", "question": "what changed?", "speak": true}"#;
        let Frame::Extension(ext) = parse_frame(raw).unwrap() else {
            panic!("expected an extension command");
        };
        assert_eq!(ext.cmd, "ask");
        assert_eq!(ext.raw, raw);
        assert_eq!(ext.fallback, Speak::default());

        let raw: &[u8] = br#"{"cmd":"nonsense","text":"hi"}"#;
        let Frame::Extension(ext) = parse_frame(raw).unwrap() else {
            panic!("expected an extension command");
        };
        assert_eq!(ext.fallback.text, "hi");
    }

    #[test]
    fn an_escaped_cmd_is_unescaped() {
        let Frame::Extension(ext) = parse_frame(br#"{"cmd":"recap"}"#).unwrap() else {
            panic!("expected an extension command");
        };
        assert_eq!(ext.cmd, "recap");
    }

    #[test]
    fn a_malformed_known_command_stays_core() {
        // `pin` without its session id does not parse as `Request::Pin`; the
        // daemon has always read that as a blank `speak`, and still does.
        let f = parse_frame(br#"{"cmd":"pin"}"#).unwrap();
        assert_eq!(f, Frame::Core(Message::Speak(Speak::default())));
        // `cancel` has no typed variant but is a core command.
        let f = parse_frame(br#"{"cmd":"cancel"}"#).unwrap();
        assert_eq!(f, Frame::Core(Message::Speak(Speak::default())));
    }

    #[test]
    fn a_non_string_cmd_is_core() {
        let f = parse_frame(br#"{"cmd":7,"text":"x"}"#).unwrap();
        assert!(matches!(f, Frame::Core(Message::Speak(_))));
    }

    #[test]
    fn not_json_is_an_error() {
        assert!(parse_frame(b"not json").is_err());
    }
}
