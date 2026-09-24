//! Words neither misaki lexicon knows.
//!
//! misaki hands out-of-lexicon words to espeak-ng. That is exactly the GPL
//! dependency this crate exists to avoid, and `sayd-misaki-en` without a
//! fallback **drops the word silently** — "I opened a PR on GitHub" would be
//! spoken as "I opened a on". For a voice that reads out an agent's work,
//! full of `JSON`, `config`, `npm` and `tokenize_utf8`, that is not an edge
//! case. So this module is Heard's own fallback, in four steps, first hit
//! wins:
//!
//! 1. **A small supplement** of words Heard says a lot that the lexicon does
//!    not have (`Kokoro`, `JSON`, `config`, `GitHub`, …), hand-written in
//!    misaki's alphabet.
//! 2. **Split compounds** — camelCase, letter/digit and hyphen boundaries —
//!    and look each part up again (`GitHub` → `Git` + `Hub`, `utf8` → `utf` +
//!    `8`, `off-by-one` → `off` + `by` + `one`).
//! 3. **Spell** anything that reads as an initialism: all-caps up to five
//!    letters, three letters or fewer, or no vowel letter at all (`PR`,
//!    `HTTP`, `utf`, `npm`, `md`), as
//!    letter names with misaki's stress pattern (secondary on all but the
//!    last, as the lexicon has `PM` → `pˌiˈɛm`).
//! 4. **Letter-to-sound rules** — a deliberately small set of English
//!    spelling rules. Plausible, not good; it exists so an unknown word is
//!    *said* rather than skipped. Every phoneme it can emit is in Kokoro's
//!    vocabulary.
//!
//! Everything here is Heard's own code under the workspace licence; none of
//! it is derived from espeak-ng.

/// Hand-written pronunciations for words Heard speaks often and misaki lacks.
/// Keys are lowercase. Stress marks sit before the vowel, as misaki's do.
const SUPPLEMENT: &[(&str, &str)] = &[
    ("async", "ˈAsɪŋk"),
    ("bool", "bˈul"),
    ("config", "kˈɑnfəɡ"),
    ("enum", "ˈinəm"),
    ("espeak", "ˈispˌik"),
    ("github", "ɡˈɪthˌʌb"),
    ("gitlab", "ɡˈɪtlˌæb"),
    ("ios", "ˌIOˈɛs"),
    ("json", "ʤˈAsᵊn"),
    ("kokoro", "kəkˈɔɹO"),
    ("macos", "mˌækOˈɛs"),
    ("misaki", "misˈɑki"),
    ("onnx", "ˈɑnɪks"),
    ("readme", "ɹˈidmi"),
    ("regex", "ɹˈɛʤɛks"),
    ("repo", "ɹˈipO"),
    ("sql", "sˈikwəl"),
    ("stderr", "stˈændəɹd ˈɛɹəɹ"),
    ("stdout", "stˈændəɹd ˈWt"),
    ("toml", "tˈɑməl"),
    ("yaml", "jˈæməl"),
];

/// Letter names, stressed. `W` is the one with two syllables.
const LETTER_NAMES: [&str; 26] = [
    "ˈA",
    "bˈi",
    "sˈi",
    "dˈi",
    "ˈi",
    "ˈɛf",
    "ʤˈi",
    "ˈAʧ",
    "ˈI",
    "ʤˈA",
    "kˈA",
    "ˈɛl",
    "ˈɛm",
    "ˈɛn",
    "ˈO",
    "pˈi",
    "kjˈu",
    "ˈɑɹ",
    "ˈɛs",
    "tˈi",
    "jˈu",
    "vˈi",
    "dˈʌbᵊlju",
    "ˈɛks",
    "wˈI",
    "zˈi",
];

/// Initialisms longer than this are more likely shouted words than letters.
const MAX_SPELLED: usize = 5;

/// The fallback entry point: `word` is one token the lexicon, the special
/// cases and the stemmer all missed. `lookup` phonemises a sub-part through
/// the lexicon (and its number normaliser) with **no** fallback, returning
/// the empty string for a miss — so this never recurses into itself through
/// the lexicon.
pub(super) fn resolve(word: &str, lookup: &dyn Fn(&str) -> String) -> Option<String> {
    if let Some(ps) = supplement(word) {
        return Some(ps);
    }
    // Possessive of an unknown word: "Kokoro's" → "Kokoro" + z.
    for suffix in ["'s", "\u{2019}s"] {
        if let Some(stem) = word.strip_suffix(suffix) {
            if !stem.is_empty() {
                return resolve(stem, lookup).map(|ps| ps + "z");
            }
        }
    }

    let parts = split_parts(word);
    if parts.len() > 1 {
        let pieces: Vec<String> = parts
            .iter()
            .filter_map(|part| {
                let known = lookup(part);
                if known.trim().is_empty() {
                    single(part)
                } else {
                    Some(known.trim().to_string())
                }
            })
            .collect();
        return (!pieces.is_empty()).then(|| pieces.join(" "));
    }
    single(word)
}

/// One part with no internal boundary left: supplement, spell, or rules.
fn single(word: &str) -> Option<String> {
    if let Some(ps) = supplement(word) {
        return Some(ps);
    }
    let letters: String = word.chars().filter(char::is_ascii_alphabetic).collect();
    if letters.is_empty() {
        return None;
    }
    let has_vowel = letters.chars().any(|c| "aeiouyAEIOUY".contains(c));
    let all_caps = letters.chars().all(|c| c.is_ascii_uppercase());
    // Three letters or fewer and still unknown after the lexicon, the
    // stemmer and the supplement is nearly always an abbreviation
    // (`utf`, `src`, `cfg`), not a word.
    if letters.len() <= 3 || !has_vowel || (all_caps && letters.len() <= MAX_SPELLED) {
        Some(spell(&letters))
    } else {
        rules(&letters.to_ascii_lowercase())
    }
}

fn supplement(word: &str) -> Option<String> {
    let lower = word.to_lowercase();
    let find = |w: &str| {
        SUPPLEMENT
            .iter()
            .find(|(k, _)| *k == w)
            .map(|(_, ps)| (*ps).to_string())
    };
    find(&lower).or_else(|| {
        // A plural of a supplement word: "configs", "repos".
        lower.strip_suffix('s').and_then(find).map(|ps| ps + "z")
    })
}

/// Split at hyphens, lower→Upper (`gitHub`), UPPER→Upper+lower (`HTTPServer`
/// → `HTTP` + `Server`) and letter↔digit boundaries.
fn split_parts(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut parts = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '-' {
            if !cur.is_empty() {
                parts.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if let Some(&prev) = i.checked_sub(1).and_then(|j| chars.get(j)) {
            let next = chars.get(i + 1).copied();
            let boundary = (prev.is_lowercase() && c.is_uppercase())
                || (prev.is_uppercase()
                    && c.is_uppercase()
                    && next.is_some_and(char::is_lowercase))
                || (prev.is_alphabetic() && c.is_ascii_digit())
                || (prev.is_ascii_digit() && c.is_alphabetic());
            if boundary && !cur.is_empty() {
                parts.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

/// Letter names, joined, secondary stress on all but the last — the shape
/// misaki's lexicon gives initialisms (`PM` → `pˌiˈɛm`).
fn spell(letters: &str) -> String {
    let names: Vec<&str> = letters
        .chars()
        .filter_map(|c| {
            let i = (c.to_ascii_lowercase() as usize).checked_sub('a' as usize)?;
            LETTER_NAMES.get(i).copied()
        })
        .collect();
    let last = names.len().saturating_sub(1);
    names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            if i == last {
                (*n).to_string()
            } else {
                n.replace('ˈ', "ˌ")
            }
        })
        .collect()
}

/// One unit of rule output.
#[derive(Debug, Clone)]
enum Unit {
    Consonant(&'static str),
    /// A vowel; `reducible` ones become `ə` when unstressed.
    Vowel {
        ps: &'static str,
        reducible: bool,
    },
}

fn is_vowel_letter(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

/// Small English letter-to-sound rules over a lowercase ASCII word.
///
/// Stress goes on the first vowel; short vowels after it reduce to `ə`. A
/// single vowel letter is "long" (`A i I O u`) when exactly one consonant
/// separates it from the next vowel letter (`Kokoro`, `base`), and short
/// (`æ ɛ ɪ ɑ ʌ`) otherwise (`clippy`, `linter`). A final `e` after a
/// consonant is silent.
fn rules(w: &str) -> Option<String> {
    use Unit::{Consonant as C, Vowel as V};
    let c: Vec<char> = w.chars().collect();
    let n = c.len();
    let at = |i: usize| c.get(i).copied();
    let starts = |i: usize, s: &str| c[i..].iter().copied().take(s.len()).eq(s.chars());
    let long = |ps| V {
        ps,
        reducible: false,
    };
    let short = |ps| V {
        ps,
        reducible: true,
    };

    let mut out: Vec<Unit> = Vec::new();
    let mut i = 0;
    while i < n {
        // (grapheme, units) — longest first, first match wins.
        let multi: &[(&str, &[Unit])] = &[
            (
                "tion",
                &[
                    C("ʃ"),
                    V {
                        ps: "ə",
                        reducible: false,
                    },
                    C("n"),
                ],
            ),
            (
                "sion",
                &[
                    C("ʒ"),
                    V {
                        ps: "ə",
                        reducible: false,
                    },
                    C("n"),
                ],
            ),
            (
                "eigh",
                &[V {
                    ps: "A",
                    reducible: false,
                }],
            ),
            ("tch", &[C("ʧ")]),
            (
                "igh",
                &[V {
                    ps: "I",
                    reducible: false,
                }],
            ),
            ("ch", &[C("ʧ")]),
            ("sh", &[C("ʃ")]),
            ("th", &[C("θ")]),
            ("ph", &[C("f")]),
            ("wh", &[C("w")]),
            ("ck", &[C("k")]),
            ("ng", &[C("ŋ")]),
            ("qu", &[C("k"), C("w")]),
            ("gh", &[]),
            (
                "ee",
                &[V {
                    ps: "i",
                    reducible: false,
                }],
            ),
            (
                "ea",
                &[V {
                    ps: "i",
                    reducible: false,
                }],
            ),
            (
                "oo",
                &[V {
                    ps: "u",
                    reducible: false,
                }],
            ),
            (
                "ou",
                &[V {
                    ps: "W",
                    reducible: false,
                }],
            ),
            (
                "ow",
                &[V {
                    ps: "O",
                    reducible: false,
                }],
            ),
            (
                "oi",
                &[V {
                    ps: "Y",
                    reducible: false,
                }],
            ),
            (
                "oy",
                &[V {
                    ps: "Y",
                    reducible: false,
                }],
            ),
            (
                "ai",
                &[V {
                    ps: "A",
                    reducible: false,
                }],
            ),
            (
                "ay",
                &[V {
                    ps: "A",
                    reducible: false,
                }],
            ),
            (
                "au",
                &[V {
                    ps: "ɔ",
                    reducible: false,
                }],
            ),
            (
                "aw",
                &[V {
                    ps: "ɔ",
                    reducible: false,
                }],
            ),
            (
                "oa",
                &[V {
                    ps: "O",
                    reducible: false,
                }],
            ),
            (
                "ie",
                &[V {
                    ps: "i",
                    reducible: false,
                }],
            ),
            (
                "ei",
                &[V {
                    ps: "A",
                    reducible: false,
                }],
            ),
            (
                "ey",
                &[V {
                    ps: "A",
                    reducible: false,
                }],
            ),
            (
                "ue",
                &[V {
                    ps: "u",
                    reducible: false,
                }],
            ),
            (
                "ew",
                &[V {
                    ps: "u",
                    reducible: false,
                }],
            ),
        ];
        if i == 0 && (starts(0, "kn") || starts(0, "wr") || starts(0, "gh")) {
            out.push(C(match c[0] {
                'k' => "n",
                'w' => "ɹ",
                _ => "ɡ",
            }));
            i += 2;
            continue;
        }
        if let Some((g, units)) = multi.iter().find(|(g, _)| starts(i, g)) {
            out.extend(units.iter().cloned());
            i += g.chars().count();
            continue;
        }

        let ch = c[i];
        // A doubled consonant is one sound: "clippy", "cc".
        if i > 0 && c[i - 1] == ch && !is_vowel_letter(ch) && ch != 'y' {
            i += 1;
            continue;
        }

        // An r-coloured vowel: vowel + r not followed by a vowel letter.
        if is_vowel_letter(ch) && at(i + 1) == Some('r') && !at(i + 2).is_some_and(is_vowel_letter)
        {
            let ps = match ch {
                'a' => "ɑ",
                'o' => "ɔ",
                _ => "ɜ",
            };
            out.push(V {
                ps,
                reducible: ps == "ɜ",
            });
            out.push(C("ɹ"));
            i += 2;
            continue;
        }

        let unit = match ch {
            'a' | 'e' | 'i' | 'o' | 'u' => {
                let final_e = ch == 'e'
                    && i == n - 1
                    && n > 2
                    && !is_vowel_letter(c[i - 1])
                    && c[..i - 1].iter().any(|&x| is_vowel_letter(x) || x == 'y');
                if final_e {
                    i += 1;
                    continue;
                }
                if i == n - 1 {
                    Some(match ch {
                        'a' => short("ə"),
                        'e' | 'i' => long("i"),
                        'o' => long("O"),
                        _ => long("u"),
                    })
                } else {
                    // Consonant letters between this vowel and the next one.
                    let gap = c[i + 1..]
                        .iter()
                        .take_while(|&&x| !is_vowel_letter(x) && x != 'y')
                        .count();
                    let reaches_vowel = i + 1 + gap < n;
                    if gap == 1 && reaches_vowel {
                        Some(long(match ch {
                            'a' => "A",
                            'e' => "i",
                            'i' => "I",
                            'o' => "O",
                            _ => "u",
                        }))
                    } else {
                        Some(short(match ch {
                            'a' => "æ",
                            'e' => "ɛ",
                            'i' => "ɪ",
                            'o' => "ɑ",
                            _ => "ʌ",
                        }))
                    }
                }
            }
            'y' => {
                if i == 0 || at(i + 1).is_some_and(is_vowel_letter) {
                    Some(C("j"))
                } else if i == n - 1 {
                    Some(long("i"))
                } else {
                    Some(short("ɪ"))
                }
            }
            'c' => Some(C(
                if at(i + 1).is_some_and(|x| matches!(x, 'e' | 'i' | 'y')) {
                    "s"
                } else {
                    "k"
                },
            )),
            'g' => Some(C(
                if at(i + 1).is_some_and(|x| matches!(x, 'e' | 'i' | 'y')) {
                    "ʤ"
                } else {
                    "ɡ"
                },
            )),
            'x' => {
                if i == 0 {
                    Some(C("z"))
                } else {
                    out.push(C("k"));
                    Some(C("s"))
                }
            }
            'j' => Some(C("ʤ")),
            'q' => Some(C("k")),
            'r' => Some(C("ɹ")),
            'b' => Some(C("b")),
            'd' => Some(C("d")),
            'f' => Some(C("f")),
            'h' => Some(C("h")),
            'k' => Some(C("k")),
            'l' => Some(C("l")),
            'm' => Some(C("m")),
            'n' => Some(C("n")),
            'p' => Some(C("p")),
            's' => Some(C("s")),
            't' => Some(C("t")),
            'v' => Some(C("v")),
            'w' => Some(C("w")),
            'z' => Some(C("z")),
            _ => None,
        };
        if let Some(u) = unit {
            out.push(u);
        }
        i += 1;
    }

    let first_vowel = out.iter().position(|u| matches!(u, V { .. }))?;
    let mut s = String::new();
    for (k, u) in out.iter().enumerate() {
        match u {
            C(ps) => s.push_str(ps),
            V { ps, reducible } => {
                if k == first_vowel {
                    s.push('ˈ');
                    s.push_str(ps);
                } else if *reducible {
                    s.push('ə');
                } else {
                    s.push_str(ps);
                }
            }
        }
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_lexicon(_: &str) -> String {
        String::new()
    }

    #[test]
    fn initialisms_are_spelled_with_misakis_stress_shape() {
        assert_eq!(spell("PR"), "pˌiˈɑɹ");
        assert_eq!(spell("npm"), "ˌɛnpˌiˈɛm");
        assert_eq!(single("HTTP").unwrap(), "ˌAʧtˌitˌipˈi");
    }

    #[test]
    fn the_supplement_wins_and_takes_plurals() {
        assert_eq!(resolve("Kokoro", &no_lexicon).unwrap(), "kəkˈɔɹO");
        assert_eq!(resolve("configs", &no_lexicon).unwrap(), "kˈɑnfəɡz");
        assert_eq!(resolve("Kokoro's", &no_lexicon).unwrap(), "kəkˈɔɹOz");
    }

    #[test]
    fn compounds_split_at_case_digit_and_hyphen_boundaries() {
        assert_eq!(split_parts("gitHub"), ["git", "Hub"]);
        assert_eq!(split_parts("HTTPServer"), ["HTTP", "Server"]);
        assert_eq!(split_parts("utf8"), ["utf", "8"]);
        assert_eq!(split_parts("off-by-one"), ["off", "by", "one"]);
        assert_eq!(split_parts("plain"), ["plain"]);
    }

    #[test]
    fn a_split_part_the_lexicon_knows_comes_from_the_lexicon() {
        let lookup = |w: &str| {
            if w == "Server" {
                "sˈɜɹvəɹ".to_string()
            } else {
                String::new()
            }
        };
        assert_eq!(
            resolve("HTTPServer", &lookup).unwrap(),
            "ˌAʧtˌitˌipˈi sˈɜɹvəɹ"
        );
    }

    #[test]
    fn rules_say_something_plausible() {
        assert_eq!(rules("clippy").unwrap(), "klˈɪpi");
        assert_eq!(rules("linter").unwrap(), "lˈɪntəɹ");
        assert_eq!(rules("base").unwrap(), "bˈAs");
        assert_eq!(rules("zork").unwrap(), "zˈɔɹk");
    }

    #[test]
    fn nothing_alphabetic_is_nothing() {
        assert_eq!(resolve("§", &no_lexicon), None);
    }

    #[test]
    fn every_emitted_phoneme_is_in_the_vocab() {
        let words = [
            "clippy",
            "zork",
            "quixotic",
            "tchotchke",
            "rhythm",
            "xylem",
            "knight",
            "wrangler",
            "eigh",
            "bougie",
            "yacht",
            "fjord",
            "cwm",
            "HTTPS",
            "abcdefghijklmnopqrstuvwxyz",
        ];
        let mut all: Vec<String> = SUPPLEMENT.iter().map(|(_, p)| (*p).to_string()).collect();
        all.extend(LETTER_NAMES.iter().map(|p| (*p).to_string()));
        all.extend(words.iter().filter_map(|w| resolve(w, &no_lexicon)));
        for ps in all {
            for ch in ps.chars() {
                assert!(
                    crate::kokoro::vocab::VOCAB.iter().any(|(v, _)| *v == ch),
                    "{ch:?} (in {ps:?}) is not a Kokoro token"
                );
            }
        }
    }
}
