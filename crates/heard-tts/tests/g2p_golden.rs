//! The default G2P's output, pinned.
//!
//! `MisakiG2p` is what decides how every local-voice utterance is pronounced,
//! so a change to it — a lexicon bump, a fallback rule, a normalisation
//! regex — must show up as a diff here rather than as a voice that quietly
//! sounds different. When a change is intended, regenerate with
//!
//! ```sh
//! HEARD_G2P_GOLDEN_PRINT=1 cargo test -p heard-tts --features kokoro \
//!   --test g2p_golden -- --nocapture
//! ```
//!
//! and review the diff by ear.

#![cfg(feature = "kokoro")]

use heard_tts::kokoro::vocab::VOCAB;
use heard_tts::kokoro::{G2p, MisakiG2p};

/// `(text, misaki phonemes)`.
const GOLDEN: &[(&str, &str)] = &[
    // The spike corpus.
    (
        "The build is green and the tests all pass.",
        "ðə bˈɪld ɪz ɡɹˈin ænd ðə tˈɛsts ˈɔl pˈæs.",
    ),
    (
        "Heard speaks your agent's work out loud, so you can look away.",
        "hˈɜɹd spˈiks jʊɹ ˈAʤᵊnts wˈɜɹk ˈWt lˈWd, sˌO ju kæn lˈʊk əwˈA.",
    ),
    (
        "Kokoro runs locally; nothing leaves this machine.",
        "kəkˈɔɹO ɹˈʌnz lˈOkəli; nˈʌθɪŋ lˈivz ðɪs məʃˈin.",
    ),
    // What an agent's summary sounds like: the out-of-lexicon fallback.
    (
        "I refactored the JSON config and opened a PR on GitHub.",
        "ˈI ɹifˈæktəɹd ðə ʤˈAsᵊn kˈɑnfəɡ ænd ˈOpᵊnd ɐ pˌiˈɑɹ ˌɔn ɡˈɪthˌʌb.",
    ),
    (
        "Claude ran npm install, then cargo clippy passed with 0 warnings.",
        "klˈɔd ɹˈæn ˌɛnpˌiˈɛm ɪnstˈɔl, ðˈɛn kˈɑɹɡO klˈɪpi pˈæst wɪð zˈɪɹO wˈɔɹnɪŋz.",
    ),
    (
        "The API returned HTTP 404 at 3:45 PM.",
        "ði ˌApˌiˈI ɹətˈɜɹnd ˌAʧtˌitˌipˈi fˈɔɹ hˈʌndɹəd fˈɔɹ æt θɹˈi fˈɔɹTi fˈIv pˌiˈɛm.",
    ),
    (
        "Fixed the off-by-one in tokenize_utf8 and bumped v2.1.0.",
        "fˈɪkst ði ˈɔf bˈI wˈʌn ɪn tˈOkənˌIz jˌutˌiˈɛf ˈAt ænd bˈʌmpt vˈɜɹʒən tˈu pˈYnt wˈʌn pˈYnt zˈɪɹO.",
    ),
    (
        "Heard read the README.md file aloud.",
        "hˈɜɹd ɹˈid ðə ɹˈidmi dˈɑt ˌɛmdˈi fˈIl əlˈWd.",
    ),
    // Numbers, currency, contractions, questions.
    (
        "It costs $5 and is 12.5% faster.",
        "ɪt kˈɔsts fˈIv dˈɑləɹz ænd ɪz twˈɛlv pˈYnt fˈIv pəɹsˈɛnt fˈæstəɹ.",
    ),
    ("Don't stop — we're almost done!", "dˈOnt stˈɑp — wɪɹ ˈɔlmOst dˈʌn!"),
    ("Did the deploy finish?", "dˈɪd ðə dəplˈY fˈɪnəʃ?"),
    (
        "Three tests failed in the HTTPServer module.",
        "θɹˈi tˈɛsts fˈAld ɪn ði ˌAʧtˌitˌipˈi sˈɜɹvəɹ mˈɑʤul.",
    ),
];

#[test]
fn misaki_output_is_pinned() {
    let g = MisakiG2p::new();
    let print = std::env::var("HEARD_G2P_GOLDEN_PRINT").is_ok();
    let mut failures = Vec::new();
    for (text, want) in GOLDEN {
        let got = g.phonemize(text, "en-us").expect("misaki never fails");
        if print {
            println!("    ({text:?}, {got:?}),");
        }
        if got != *want {
            failures.push(format!("{text:?}\n   want {want:?}\n    got {got:?}"));
        }
    }
    assert!(
        print || failures.is_empty(),
        "{} golden mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Kokoro's vocabulary *is* misaki's alphabet, so the pinned output must
/// survive `vocab::known` with nothing dropped — a dropped character would be
/// a phoneme the model never hears.
#[test]
fn every_pinned_phoneme_is_a_kokoro_token() {
    let g = MisakiG2p::new();
    for (text, _) in GOLDEN {
        let got = g.phonemize(text, "en-us").unwrap();
        for ch in got.chars() {
            assert!(
                VOCAB.iter().any(|(v, _)| *v == ch),
                "{ch:?} in the phonemes for {text:?} is not in Kokoro's vocabulary"
            );
        }
    }
}

/// No word is silently dropped: every word in the input contributes at least
/// one phoneme group. Without the fallback, `sayd-misaki-en` turns "a PR on
/// GitHub" into "a on".
#[test]
fn no_word_is_silently_dropped() {
    let g = MisakiG2p::new();
    for text in [
        "a PR on GitHub",
        "zxqv blorptastic frobnicate",
        "Kokoro's config",
        "the rustc stdout",
    ] {
        let got = g.phonemize(text, "en-us").unwrap();
        let groups = got.split_whitespace().count();
        let words = text.split_whitespace().count();
        assert!(
            groups >= words,
            "{text:?} -> {got:?}: {groups} groups for {words} words"
        );
    }
}

/// Odd input is phonemised or dropped, never a panic — the daemon feeds this
/// whatever an agent printed.
#[test]
fn odd_input_does_not_panic() {
    let g = MisakiG2p::new();
    for text in [
        "🎉🎉",
        "日本語のテキスト",
        "''",
        "-",
        "a-",
        "--flag",
        "123abc",
        "ÀÉÎõü",
        "x.y.z",
        "v1.",
        "v.2",
        "12:",
        ":30",
        "__init__",
        "C++ and C#",
        "e.g. i.e. etc.",
        "$",
        "%%",
        "https://example.com/a_b?c=d",
        "   \t\n  ",
    ] {
        let out = g.phonemize(text, "en-us").expect("never an error");
        for ch in out.chars() {
            assert!(
                VOCAB.iter().any(|(v, _)| *v == ch),
                "{ch:?} from {text:?} -> {out:?}"
            );
        }
    }
}

/// Every character misaki's two lexicons can emit is a Kokoro token — the
/// "no mapping table needed" claim, on the alphabet itself. The 46 characters
/// were enumerated from misaki 0.9.4's `us_gold.json` + `us_silver.json`;
/// `ɾ` and `ʔ` are in the lexicon but misaki (and `sayd-misaki-en`) rewrite
/// them to `T` and `t` as the last step, and both of those are tokens too.
#[test]
fn every_lexicon_character_is_in_the_vocab() {
    const LEXICON_ALPHABET: &str = "AIOWYbdfhijklmnpstuvwzæðŋɑɔəɛɜɡɪɹɾʃʊʌʒʔʤʧˈˌθᵊᵻ";
    assert_eq!(LEXICON_ALPHABET.chars().count(), 46);
    for ch in LEXICON_ALPHABET.chars() {
        assert!(
            VOCAB.iter().any(|(v, _)| *v == ch),
            "{ch:?} is not a Kokoro token"
        );
    }
    assert!(VOCAB.iter().any(|(v, _)| *v == 'T'));
}
