//! The Kokoro phoneme vocabulary and the tokenizer over it.
//!
//! Lifted from `kokoro_onnx/config.json`'s `vocab` object — 114 entries, all of
//! them a single `char` — so the table is a `const` slice here rather than a
//! file this crate has to ship and parse. `tests::vocab_matches_kokoro_onnx`
//! pins it against the installed Python package when one is present, so a
//! future upstream change is a test failure rather than a wrong voice.
//!
//! The mapping matters more than it looks: the ids are *not* dense (they run to
//! 177 with gaps), so an off-by-one in transcription would not fail loudly —
//! it would synthesise the wrong phoneme. Hence the pin.

/// `(phoneme, token id)`, exactly `kokoro_onnx`'s `DEFAULT_VOCAB`.
pub const VOCAB: &[(char, i64)] = &[
    (';', 1),
    (':', 2),
    (',', 3),
    ('.', 4),
    ('!', 5),
    ('?', 6),
    ('—', 9),
    ('…', 10),
    ('"', 11),
    ('(', 12),
    (')', 13),
    ('“', 14),
    ('”', 15),
    (' ', 16),
    ('\u{303}', 17),
    ('ʣ', 18),
    ('ʥ', 19),
    ('ʦ', 20),
    ('ʨ', 21),
    ('ᵝ', 22),
    ('ꭧ', 23),
    ('A', 24),
    ('I', 25),
    ('O', 31),
    ('Q', 33),
    ('S', 35),
    ('T', 36),
    ('W', 39),
    ('Y', 41),
    ('ᵊ', 42),
    ('a', 43),
    ('b', 44),
    ('c', 45),
    ('d', 46),
    ('e', 47),
    ('f', 48),
    ('h', 50),
    ('i', 51),
    ('j', 52),
    ('k', 53),
    ('l', 54),
    ('m', 55),
    ('n', 56),
    ('o', 57),
    ('p', 58),
    ('q', 59),
    ('r', 60),
    ('s', 61),
    ('t', 62),
    ('u', 63),
    ('v', 64),
    ('w', 65),
    ('x', 66),
    ('y', 67),
    ('z', 68),
    ('ɑ', 69),
    ('ɐ', 70),
    ('ɒ', 71),
    ('æ', 72),
    ('β', 75),
    ('ɔ', 76),
    ('ɕ', 77),
    ('ç', 78),
    ('ɖ', 80),
    ('ð', 81),
    ('ʤ', 82),
    ('ə', 83),
    ('ɚ', 85),
    ('ɛ', 86),
    ('ɜ', 87),
    ('ɟ', 90),
    ('ɡ', 92),
    ('ɥ', 99),
    ('ɨ', 101),
    ('ɪ', 102),
    ('ʝ', 103),
    ('ɯ', 110),
    ('ɰ', 111),
    ('ŋ', 112),
    ('ɳ', 113),
    ('ɲ', 114),
    ('ɴ', 115),
    ('ø', 116),
    ('ɸ', 118),
    ('θ', 119),
    ('œ', 120),
    ('ɹ', 123),
    ('ɾ', 125),
    ('ɻ', 126),
    ('ʁ', 128),
    ('ɽ', 129),
    ('ʂ', 130),
    ('ʃ', 131),
    ('ʈ', 132),
    ('ʧ', 133),
    ('ʊ', 135),
    ('ʋ', 136),
    ('ʌ', 138),
    ('ɣ', 139),
    ('ɤ', 140),
    ('χ', 142),
    ('ʎ', 143),
    ('ʒ', 147),
    ('ʔ', 148),
    ('ˈ', 156),
    ('ˌ', 157),
    ('ː', 158),
    ('ʰ', 162),
    ('ʲ', 164),
    ('↓', 169),
    ('→', 171),
    ('↗', 172),
    ('↘', 173),
    ('ᵻ', 177),
];

/// The longest phoneme string the model accepts. `MAX_PHONEME_LENGTH`.
pub const MAX_PHONEME_LENGTH: usize = 510;

/// The id for one phoneme, or `None` if it is outside the vocabulary.
#[must_use]
pub fn token_for(ch: char) -> Option<i64> {
    VOCAB.iter().find(|(c, _)| *c == ch).map(|(_, id)| *id)
}

/// Map phonemes to token ids, dropping anything outside the vocabulary.
///
/// `Tokenizer.tokenize`. The drop is silent and deliberate: espeak emits marks
/// (language-switch flags, some diacritics) the model was never trained on, and
/// the Python filters them twice — once in `phonemize` and once here.
///
/// Returns `None` when the input exceeds [`MAX_PHONEME_LENGTH`], which is the
/// Python's `ValueError("text is too long…")`. The caller splits and batches;
/// see [`super::split_phonemes`].
#[must_use]
pub fn tokenize(phonemes: &str) -> Option<Vec<i64>> {
    if phonemes.chars().count() > MAX_PHONEME_LENGTH {
        return None;
    }
    Some(phonemes.chars().filter_map(token_for).collect())
}

/// The phonemes that survive tokenization, aligned one to one with it.
/// `Tokenizer.known`.
#[must_use]
pub fn known(phonemes: &str) -> String {
    phonemes
        .chars()
        .filter(|c| token_for(*c).is_some())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_the_expected_size_and_has_no_duplicates() {
        assert_eq!(VOCAB.len(), 114);
        let mut chars: Vec<char> = VOCAB.iter().map(|(c, _)| *c).collect();
        chars.sort_unstable();
        chars.dedup();
        assert_eq!(chars.len(), VOCAB.len(), "a phoneme appears twice");

        let mut ids: Vec<i64> = VOCAB.iter().map(|(_, i)| *i).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), VOCAB.len(), "a token id appears twice");
    }

    #[test]
    fn ids_are_sparse_which_is_why_the_table_is_pinned() {
        // Running to 177 across 114 entries: the gaps are real, so a
        // transcription slip would mis-speak rather than fail.
        assert_eq!(token_for('ᵻ'), Some(177));
        assert_eq!(token_for('ˈ'), Some(156));
        assert!(VOCAB.iter().all(|(_, id)| *id >= 1 && *id <= 177));
    }

    #[test]
    fn tokenizing_the_ground_truth_sentence_gives_the_python_ids() {
        // From `kokoro_onnx.Tokenizer` on "The build is green and the tests
        // all pass." — captured with the model NOT loaded.
        let phonemes = "ðə bˈɪld ɪz ɡɹˈiːn ænd ðə tˈɛsts ˈɔːl pˈæs.";
        let want: Vec<i64> = vec![
            81, 83, 16, 44, 156, 102, 54, 46, 16, 102, 68, 16, 92, 123, 156, 51, 158, 56, 16, 72,
            56, 46, 16, 81, 83, 16, 62, 156, 86, 61, 62, 61, 16, 156, 76, 158, 54, 16, 58, 156, 72,
            61, 4,
        ];
        assert_eq!(tokenize(phonemes).unwrap(), want);
        assert_eq!(want.len(), 43);
    }

    #[test]
    fn unknown_characters_are_dropped_not_rejected() {
        // `(en)` language-switch flags and stray ASCII are what espeak emits
        // and the model has never seen.
        assert_eq!(tokenize("ð\u{0}ə").unwrap(), vec![81, 83]);
        assert_eq!(known("ð\u{0}ə"), "ðə");
    }

    #[test]
    fn over_length_input_is_none_not_a_panic() {
        let long = "a".repeat(MAX_PHONEME_LENGTH + 1);
        assert!(tokenize(&long).is_none());
        let ok = "a".repeat(MAX_PHONEME_LENGTH);
        assert_eq!(tokenize(&ok).unwrap().len(), MAX_PHONEME_LENGTH);
    }

    /// Pin the transcribed table against the installed `kokoro_onnx`, when the
    /// spike's venv is present. Skipped otherwise, so CI without it still
    /// passes.
    #[test]
    fn vocab_matches_kokoro_onnx() {
        let Ok(path) = std::env::var("HEARD_KOKORO_CONFIG_JSON") else {
            return;
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return;
        };
        let json: serde_json::Value = serde_json::from_str(&raw).expect("config.json");
        let upstream = json["vocab"].as_object().expect("vocab object");

        assert_eq!(upstream.len(), VOCAB.len(), "vocab size drifted");
        for (k, v) in upstream {
            let mut chars = k.chars();
            let ch = chars.next().expect("non-empty key");
            assert!(chars.next().is_none(), "multi-char vocab key {k:?}");
            assert_eq!(
                token_for(ch),
                Some(v.as_i64().expect("integer id")),
                "id for {ch:?} drifted"
            );
        }
    }
}
