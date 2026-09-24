//! Grapheme-to-phoneme: text in, Kokoro phonemes out.
//!
//! # The default, and why
//!
//! [`MisakiG2p`] — misaki's English G2P, in process, via the `sayd-misaki-en`
//! crate. It is the default for [`super::KokoroTts::new`].
//!
//! * **Licence.** `sayd-misaki-en` 0.1.0 is Apache-2.0, and so are the two
//!   lexicons it embeds (`us_gold.json`, `us_silver.json`), which are copied
//!   verbatim from misaki 0.9.4 (hexgrad, Apache-2.0) — verified byte for byte
//!   against the PyPI `misaki-0.9.4` wheel by SHA-256, see
//!   `THIRD-PARTY-NOTICES.md`. No GPL code is linked, invoked or shipped.
//! * **Alphabet.** Kokoro v1.0 was *trained* on misaki phonemes, and its
//!   114-entry vocabulary ([`super::vocab`]) is misaki's alphabet — `A I O W
//!   Y` for the diphthongs, `ʤ ʧ` for the affricates, `ᵊ` for the reduced
//!   schwa, `T` for the flap. So there is **no mapping table**: every
//!   character either lexicon can emit is already a vocabulary entry (the
//!   test `every_lexicon_character_is_in_the_vocab` pins the 46 of them).
//! * **Pronunciation differs from the Python.** `kokoro-onnx` phonemises with
//!   espeak-ng, not misaki, so every utterance comes out in a slightly
//!   different phoneme string. That difference is accepted.
//!
//! # The opt-in reference
//!
//! [`EspeakCliG2p`] (feature `espeak`, **off by default**) runs an
//! `espeak-ng` binary as a subprocess and reproduces the Python pipeline
//! exactly. It is there to diff against, not to ship: espeak-ng is GPLv3, so
//! the default build does not compile this module at all.
//!
//! [`G2p`] is a trait so the choice stays swappable without touching the
//! backend.

#[cfg(feature = "espeak")]
pub mod espeak;
pub mod misaki;
mod oov;

#[cfg(feature = "espeak")]
pub use espeak::{restore_punctuation, EspeakCliG2p};
pub use misaki::MisakiG2p;

use super::KokoroError;

/// Turn text into the phoneme string Kokoro's vocabulary is written in.
pub trait G2p: Send + Sync {
    /// Phonemise `text` in `lang` (`en-us` or `en-gb`).
    ///
    /// # Errors
    /// [`KokoroError::G2p`] when the phonemiser is missing or fails.
    fn phonemize(&self, text: &str, lang: &str) -> Result<String, KokoroError>;
}

/// A phonemiser that hands back whatever it was given.
///
/// This is how the spike separates two questions that would otherwise be one:
/// "does the ONNX graph run correctly under `ort`" and "does Rust G2P agree
/// with Python's". Feeding the model phonemes captured from the Python answers
/// the first on its own, with no G2P in the way at all.
#[derive(Debug, Clone, Default)]
pub struct PhonemesAsGiven;

impl G2p for PhonemesAsGiven {
    fn phonemize(&self, text: &str, _lang: &str) -> Result<String, KokoroError> {
        Ok(text.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_hands_back_what_it_was_given() {
        let g = PhonemesAsGiven;
        assert_eq!(g.phonemize("  ðə bˈɪld  ", "en-us").unwrap(), "ðə bˈɪld");
    }
}
