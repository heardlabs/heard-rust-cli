//! misaki's English G2P, in process — the default phonemiser.
//!
//! Wraps `sayd-misaki-en` (Apache-2.0; lexicons verbatim from misaki 0.9.4,
//! Apache-2.0 — see [`super`] and `THIRD-PARTY-NOTICES.md`) and adds the two
//! things it leaves to the caller:
//!
//! * **An out-of-lexicon fallback** that is not espeak ([`super::oov`]), so an
//!   unknown word is said rather than silently dropped.
//! * **A little text normalisation** for the shapes an agent's output is full
//!   of and prose is not: `snake_case` (underscores become spaces),
//!   `README.md` (a dot between a word and a short extension is read "dot"),
//!   `v2.1.0` ("version 2 point 1 point 0") and clock times (`3:45` →
//!   "3 45", `9:00` → "9 o'clock").
//!
//! # Known limits, stated rather than hidden
//!
//! * **US English only.** Only the US lexicons are vendored, so `en-gb`
//!   phonemises as American. British Kokoro voices (`bf_*`, `bm_*`) will get
//!   American phonemes; they still speak, with an accent mismatch.
//! * **No part-of-speech tagger.** Homographs take misaki's `DEFAULT` reading
//!   ("read" is always /ɹˈid/). The upstream port measures this at ~1% of
//!   words.
//! * **The fallback is rules, not a model.** Unknown non-initialism words are
//!   pronounced by a small set of spelling rules — intelligible, not native.

use once_cell::sync::Lazy;
use regex::Regex;

use super::{oov, G2p};
use crate::kokoro::KokoroError;

/// `3:45` — a clock time. misaki would keep the colon, which Kokoro reads as
/// a pause mid-number.
static CLOCK: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(\d{1,2}):(\d{2})\b").expect("static regex"));
/// `v2.1.0` and friends.
static VERSION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bv(\d+(?:\.\d+)+)\b").expect("static regex"));
/// `README.md`, `main.rs`, `example.com` — a word of 2+ letters, a dot, and a
/// short alphanumeric extension. Two letters minimum so `e.g.` is left alone.
static DOTTED: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\b([A-Za-z][A-Za-z0-9]+)\.([A-Za-z][A-Za-z0-9]{0,3})\b").expect("static regex")
});

/// misaki via `sayd-misaki-en`, with Heard's espeak-free fallback.
pub struct MisakiG2p {
    inner: sayd_misaki_en::G2p,
}

impl std::fmt::Debug for MisakiG2p {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MisakiG2p")
            .field("version", &sayd_misaki_en::version())
            .finish()
    }
}

impl Default for MisakiG2p {
    fn default() -> Self {
        Self::new()
    }
}

impl MisakiG2p {
    /// Build the phonemiser. Cheap — the lexicons are FSTs compiled into the
    /// binary, so this is two map views over static bytes, no parsing.
    #[must_use]
    pub fn new() -> Self {
        // The fallback looks sub-parts up through a second, fallback-free
        // instance, so a miss inside the fallback is a miss, not a recursion.
        let plain = sayd_misaki_en::G2p::new(false);
        let inner = sayd_misaki_en::G2p::with_fallback(
            false,
            Box::new(move |word| oov::resolve(word, &|part| plain.phonemize(part))),
        );
        Self { inner }
    }
}

/// Rewrite the non-prose shapes before misaki sees them.
fn normalise(text: &str) -> String {
    let text = text.replace('_', " ");
    let text = CLOCK.replace_all(&text, |c: &regex::Captures<'_>| {
        if &c[2] == "00" {
            format!("{} o'clock", &c[1])
        } else {
            format!("{} {}", &c[1], &c[2])
        }
    });
    let text = VERSION.replace_all(&text, |c: &regex::Captures<'_>| {
        format!("version {}", c[1].replace('.', " point "))
    });
    DOTTED.replace_all(&text, "$1 dot $2").into_owned()
}

impl G2p for MisakiG2p {
    fn phonemize(&self, text: &str, _lang: &str) -> Result<String, KokoroError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(String::new());
        }
        Ok(self.inner.phonemize(&normalise(trimmed)).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation_reads_code_shapes_aloud() {
        assert_eq!(normalise("tokenize_utf8"), "tokenize utf8");
        assert_eq!(
            normalise("bumped v2.1.0."),
            "bumped version 2 point 1 point 0."
        );
        assert_eq!(normalise("the README.md file"), "the README dot md file");
        assert_eq!(normalise("e.g. this"), "e.g. this");
        assert_eq!(normalise("at 3:45 PM"), "at 3 45 PM");
        assert_eq!(normalise("at 9:00"), "at 9 o'clock");
        assert_eq!(normalise("It passed. Then"), "It passed. Then");
    }

    #[test]
    fn empty_text_is_empty() {
        assert_eq!(MisakiG2p::new().phonemize("  ", "en-us").unwrap(), "");
    }
}
