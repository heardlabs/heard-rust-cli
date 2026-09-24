//! The "no voice configured" backend — ported from `engine/heard/tts/null.py`.
//!
//! Selected when no registered backend claims the voice, the user carries no
//! ElevenLabs key of their own, *and* has not explicitly downloaded the local
//! Kokoro model. Heard used to fall through to Kokoro here, which meant a
//! brand-new user's first agent output triggered an unannounced ~325 MB
//! download; that download is now opt-in, so the fallback is this: no audio,
//! plus a one-time nudge telling the user how to get a voice.
//!
//! It implements [`Tts`] in full so the daemon can hold one in its `tts` field
//! without special-casing every attribute access — but [`NullTts::synth`]
//! always fails, and the speech worker checks [`Tts::is_configured`] up front
//! so it never actually gets there. In the Python that check is
//! `isinstance(..., NullTTS)`; a trait method is the same test without the
//! downcast.

use crate::{Audio, Tts, TtsError};

/// Synthesis was attempted with no voice backend configured.
///
/// Belt-and-braces: the speech worker should detect an unconfigured backend
/// before calling [`Tts::synth`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "No voice configured — sign in to Heard, add an ElevenLabs key, \
     or download the local voice (Options → Download voice)."
)]
pub struct NullTtsError;

/// The silent backend.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NullTts;

impl Tts for NullTts {
    /// `.mp3`, matching `NullTTS.AUDIO_EXT` — the daemon still mints a
    /// tempfile before it discovers there is no voice.
    fn audio_ext(&self) -> &'static str {
        ".mp3"
    }

    /// `1.0`, matching `NullTTS.MAX_NATIVE_SPEED`.
    fn max_native_speed(&self) -> f64 {
        1.0
    }

    /// Always `false` — this is the signal the speech worker reads.
    fn is_configured(&self) -> bool {
        false
    }

    fn synth(
        &self,
        _text: &str,
        _voice: &str,
        _speed: f64,
        _lang: &str,
    ) -> Result<Audio, TtsError> {
        Err(TtsError::Null(NullTtsError))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn surface_matches_the_python_class_attributes() {
        let t = NullTts;
        assert_eq!(t.audio_ext(), ".mp3");
        assert!((t.max_native_speed() - 1.0).abs() < f64::EPSILON);
        assert!(!t.is_configured());
        assert!(t.list_voices().is_empty());
    }

    #[test]
    fn synth_is_a_sentence_naming_all_three_ways_out() {
        let err = NullTts.synth("hello", "george", 1.0, "en-us").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("No voice configured"));
        assert!(msg.contains("sign in to Heard"));
        assert!(msg.contains("ElevenLabs key"));
        assert!(msg.contains("Options → Download voice"));
        assert!(matches!(err, TtsError::Null(NullTtsError)));
    }

    #[test]
    fn synth_to_file_fails_without_creating_the_file() {
        let path = std::env::temp_dir().join("heard-tts-null-must-not-exist.mp3");
        let _ = std::fs::remove_file(&path);
        assert!(NullTts
            .synth_to_file("hello", "", 1.0, "en-us", &path)
            .is_err());
        assert!(!path.exists());
    }

    #[test]
    fn is_usable_as_a_trait_object() {
        let t: Box<dyn Tts> = Box::new(NullTts);
        assert!(!t.is_configured());
        assert!(t.synth("x", "", 1.0, "en-us").is_err());
        let _ = Path::new("/tmp");
    }
}
