//! The espeak-ng phonemiser, invoked as a **subprocess** — opt-in only.
//!
//! **Behind the `espeak` cargo feature, off by default.** The default G2P is
//! [`super::MisakiG2p`] (in-process, Apache-2.0). This impl is kept because it
//! is the one route that reproduces the *Python* `kokoro-onnx` pipeline
//! character for character, which makes it the reference to diff against when
//! someone asks "did the Rust port change how X is said?".
//!
//! # Licence position
//!
//! espeak-ng is **GPL-3.0-or-later**. This module contains no espeak code and
//! links nothing: it runs an `espeak-ng` binary the user (or a build) provides
//! and reads its stdout. Even so, the default build and the shipped bundle do
//! not include it at all — nothing compiles unless `--features espeak` is
//! passed, and `tests/no_espeak_by_default.rs` pins that the default
//! dependency tree contains no espeak crate. Shipping an `espeak-ng` binary
//! next to Heard would be a GPL distribution question; this feature exists so
//! that question never has to be asked of the default build.
//!
//! # What the Python actually does
//!
//! `kokoro_onnx.Tokenizer.phonemize` is three lines:
//!
//! ```python
//! phonemes = phonemizer.phonemize(text, lang, preserve_punctuation=True, with_stress=True)
//! phonemes = "".join(filter(lambda p: p in self.vocab, phonemes))
//! return phonemes.strip()
//! ```
//!
//! `phonemizer`'s espeak backend wraps **libespeak-ng**. `espeak-ng -v en-us
//! -q --ipa` reproduces its output exactly; the only difference is that the
//! CLI breaks on punctuation and drops the mark, which is precisely the layer
//! `preserve_punctuation=True` adds back — [`restore_punctuation`] puts it
//! back. The cost is one process spawn per utterance (~20 ms median on the
//! test machine).

use std::process::Command;

use super::G2p;
use crate::kokoro::KokoroError;

/// espeak-ng, invoked as a subprocess.
#[derive(Debug, Clone)]
pub struct EspeakCliG2p {
    binary: String,
}

impl Default for EspeakCliG2p {
    fn default() -> Self {
        Self::new()
    }
}

impl EspeakCliG2p {
    /// Use whatever `espeak-ng` is on `PATH`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            binary: "espeak-ng".to_string(),
        }
    }

    /// Use a specific binary — Homebrew's, say.
    #[must_use]
    pub fn with_binary(binary: &str) -> Self {
        Self {
            binary: binary.to_string(),
        }
    }

    /// Whether the binary is there and runs.
    #[must_use]
    pub fn is_available(&self) -> bool {
        Command::new(&self.binary)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

/// The punctuation espeak breaks a line on and drops, which
/// `preserve_punctuation=True` puts back. Taken from what the CLI actually
/// splits on for the spike corpus, intersected with the marks that are in
/// Kokoro's vocabulary — a mark the model cannot tokenise is not worth
/// restoring.
const BREAKING_PUNCTUATION: &[char] = &[',', '.', ';', ':', '!', '?', '—', '…'];

impl G2p for EspeakCliG2p {
    fn phonemize(&self, text: &str, lang: &str) -> Result<String, KokoroError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(String::new());
        }

        let out = Command::new(&self.binary)
            .args(["-v", lang, "-q", "--ipa", "--", trimmed])
            .output()
            .map_err(|e| {
                KokoroError::G2p(format!(
                    "espeak-ng is not available ({e}) — the local voice needs it; \
                     install it or pick a cloud voice"
                ))
            })?;

        if !out.status.success() {
            return Err(KokoroError::G2p(format!(
                "espeak-ng exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }

        let raw = String::from_utf8_lossy(&out.stdout);
        Ok(restore_punctuation(trimmed, &raw))
    }
}

/// Re-attach the punctuation espeak dropped, joining its per-clause lines.
///
/// espeak prints one line per clause and eats the mark that ended it;
/// phonemizer's `preserve_punctuation` keeps the mark. Walking the source text
/// for breaking marks in order and appending the nth one to the nth line
/// reproduces that, because espeak breaks on exactly those marks — which is
/// what makes this a restoration rather than a guess.
#[must_use]
pub fn restore_punctuation(source: &str, espeak_output: &str) -> String {
    let lines: Vec<&str> = espeak_output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return String::new();
    }

    let marks: Vec<char> = source
        .chars()
        .filter(|c| BREAKING_PUNCTUATION.contains(c))
        .collect();

    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(line);
        if let Some(mark) = marks.get(i) {
            out.push(*mark);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn punctuation_is_restored_clause_by_clause() {
        // What espeak prints for sentence 2 of the spike corpus, and what
        // phonemizer returns for it.
        let source = "Heard speaks your agent's work out loud, so you can look away.";
        let espeak = "hˈɜːd spˈiːks jʊɹ ˈeɪdʒənts wˈɜːk ˈaʊt lˈaʊd\nsˌoʊ juː kæn lˈʊk ɐwˈeɪ\n";
        assert_eq!(
            restore_punctuation(source, espeak),
            "hˈɜːd spˈiːks jʊɹ ˈeɪdʒənts wˈɜːk ˈaʊt lˈaʊd, sˌoʊ juː kæn lˈʊk ɐwˈeɪ."
        );
    }

    #[test]
    fn a_semicolon_is_restored_as_itself() {
        let source = "Kokoro runs locally; nothing leaves this machine.";
        let espeak = "kəkˈɔːɹoʊ ɹˈʌnz lˈoʊkəli\nnˈʌθɪŋ lˈiːvz ðɪs məʃˈiːn\n";
        assert_eq!(
            restore_punctuation(source, espeak),
            "kəkˈɔːɹoʊ ɹˈʌnz lˈoʊkəli; nˈʌθɪŋ lˈiːvz ðɪs məʃˈiːn."
        );
    }

    #[test]
    fn a_single_clause_still_gets_its_final_mark() {
        let source = "The build is green and the tests all pass.";
        let espeak = "ðə bˈɪld ɪz ɡɹˈiːn ænd ðə tˈɛsts ˈɔːl pˈæs\n";
        assert_eq!(
            restore_punctuation(source, espeak),
            "ðə bˈɪld ɪz ɡɹˈiːn ænd ðə tˈɛsts ˈɔːl pˈæs."
        );
    }

    #[test]
    fn text_with_no_punctuation_gains_none() {
        assert_eq!(
            restore_punctuation("all green", "ˈɔːl ɡɹˈiːn\n"),
            "ˈɔːl ɡɹˈiːn"
        );
    }

    #[test]
    fn empty_espeak_output_is_empty_not_a_stray_mark() {
        assert_eq!(restore_punctuation("...", ""), "");
    }

    #[test]
    fn a_missing_binary_is_a_sentence_naming_the_way_out() {
        let g = EspeakCliG2p::with_binary("espeak-ng-does-not-exist-here");
        assert!(!g.is_available());
        let err = g.phonemize("hello", "en-us").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("espeak-ng is not available"), "{msg}");
        assert!(msg.contains("pick a cloud voice"), "{msg}");
    }

    #[test]
    fn empty_text_does_not_spawn_anything() {
        // Even with a binary that does not exist: the empty check comes first.
        let g = EspeakCliG2p::with_binary("espeak-ng-does-not-exist-here");
        assert_eq!(g.phonemize("   ", "en-us").unwrap(), "");
    }

    /// espeak-ng's CLI plus punctuation restoration equals
    /// `phonemizer.phonemize(..., preserve_punctuation=True, with_stress=True)`.
    /// Gated on the binary being present.
    #[test]
    fn cli_output_matches_the_python_phonemizer_for_the_spike_corpus() {
        let g = EspeakCliG2p::new();
        if !g.is_available() {
            return; // no espeak-ng here; the pin cannot run
        }
        let cases = [
            (
                "The build is green and the tests all pass.",
                "ðə bˈɪld ɪz ɡɹˈiːn ænd ðə tˈɛsts ˈɔːl pˈæs.",
            ),
            (
                "Heard speaks your agent's work out loud, so you can look away.",
                "hˈɜːd spˈiːks jʊɹ ˈeɪdʒənts wˈɜːk ˈaʊt lˈaʊd, sˌoʊ juː kæn lˈʊk ɐwˈeɪ.",
            ),
            (
                "Kokoro runs locally; nothing leaves this machine.",
                "kəkˈɔːɹoʊ ɹˈʌnz lˈoʊkəli; nˈʌθɪŋ lˈiːvz ðɪs məʃˈiːn.",
            ),
        ];
        for (text, want) in cases {
            assert_eq!(g.phonemize(text, "en-us").unwrap(), want, "for {text:?}");
        }
    }
}
