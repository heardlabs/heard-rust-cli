//! Split a phoneme string into batches the Kokoro graph can take in one
//! pass — `kokoro_onnx/chunker.py` (kokoro-onnx 0.6.1, the version the Python
//! app ships), ported line for line.
//!
//! The graph's context is fixed at 510 phonemes, so long input is cut at the
//! least disruptive boundary available: sentence, then clause, then word,
//! and only as a last resort mid-word. The batches are then *balanced*: the
//! smallest limit that needs no more batches than filling each to the brim,
//! because a short trailing batch is spoken at a different rate and loudness
//! than its neighbours.
//!
//! Pure string work, so it is not behind the `kokoro` feature;
//! `tests/chunk_golden.rs` pins it against the real Python.

/// Marks that end a sentence (`SENTENCE_MARKS`).
pub const SENTENCE_MARKS: &str = ".!?…";
/// Marks that end a clause (`CLAUSE_MARKS`).
pub const CLAUSE_MARKS: &str = ",;:";

/// Seconds of silence after a batch ending in a sentence mark
/// (`create(sentence_pause=0.25)`).
pub const SENTENCE_PAUSE: f64 = 0.25;
/// Seconds of silence after a batch ending in a clause mark
/// (`create(clause_pause=0.1)`).
pub const CLAUSE_PAUSE: f64 = 0.1;

/// Python's `str.isspace` for one character (what `\s` and `str.split()`
/// match in a `str` pattern): Rust's whitespace plus the four ASCII
/// separators Python also counts.
#[must_use]
pub fn py_isspace(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

/// `" ".join(phonemes.split())` — what `_prepare` does before splitting, so
/// newlines (not in the vocabulary) become the space the model pauses on.
#[must_use]
pub fn normalize(phonemes: &str) -> String {
    phonemes
        .split(py_isspace)
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `_BOUNDARIES[level].split(s)`: level 0 splits at whitespace runs that
/// follow a sentence mark, level 1 after a clause mark, level 2 at every
/// whitespace run. (`re.split` with a look-behind: the mark stays with the
/// text before it, and a run is only a boundary when the character right
/// before it is a mark.)
fn split_level(s: &str, level: usize) -> Vec<&str> {
    let marks = match level {
        0 => Some(SENTENCE_MARKS),
        1 => Some(CLAUSE_MARKS),
        _ => None,
    };
    let mut out = Vec::new();
    let mut piece_start = 0usize;
    let mut prev: Option<char> = None;
    let mut iter = s.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        let boundary = py_isspace(c)
            && match marks {
                Some(m) => prev.is_some_and(|p| m.contains(p)),
                None => true,
            };
        if !boundary {
            prev = Some(c);
            continue;
        }
        // Consume the whole whitespace run.
        let mut end = i + c.len_utf8();
        while let Some(&(j, d)) = iter.peek() {
            if !py_isspace(d) {
                break;
            }
            end = j + d.len_utf8();
            iter.next();
        }
        out.push(&s[piece_start..i]);
        piece_start = end;
        prev = None;
    }
    out.push(&s[piece_start..]);
    out
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// `_atoms(phonemes, max_length, level)`.
fn atoms(phonemes: &str, max_length: usize, level: usize, out: &mut Vec<String>) {
    if char_len(phonemes) <= max_length {
        if !phonemes.is_empty() {
            out.push(phonemes.to_string());
        }
        return;
    }
    for index in level..3 {
        let pieces = split_level(phonemes, index);
        if pieces.len() > 1 {
            for piece in pieces {
                atoms(py_strip(piece), max_length, index + 1, out);
            }
            return;
        }
    }
    // A single unbroken run longer than the context: slice it rather than
    // dropping the tail.
    let chars: Vec<char> = phonemes.chars().collect();
    for chunk in chars.chunks(max_length.max(1)) {
        out.push(chunk.iter().collect());
    }
}

/// `_pack(lengths, limit)` — consecutive atoms grouped within `limit`
/// (joined by one space each), as index ranges.
fn pack(lengths: &[usize], limit: usize) -> Vec<(usize, usize)> {
    let mut batches = Vec::new();
    let (mut start, mut size) = (0usize, 0usize);
    for (index, &length) in lengths.iter().enumerate() {
        let candidate = if index == start {
            length
        } else {
            size + 1 + length
        };
        if candidate > limit && index > start {
            batches.push((start, index));
            start = index;
            size = length;
        } else {
            size = candidate;
        }
    }
    if !lengths.is_empty() {
        batches.push((start, lengths.len()));
    }
    batches
}

/// `split_phonemes(phonemes, max_length)` — evenly sized batches of at most
/// `max_length` characters (Python code points).
#[must_use]
pub fn split_phonemes_max(phonemes: &str, max_length: usize) -> Vec<String> {
    let mut atom_list = Vec::new();
    atoms(py_strip(phonemes), max_length, 0, &mut atom_list);
    if atom_list.is_empty() {
        return Vec::new();
    }
    let lengths: Vec<usize> = atom_list.iter().map(|a| char_len(a)).collect();
    let fewest = pack(&lengths, max_length).len();
    let (mut low, mut high) = (lengths.iter().copied().max().unwrap_or(0), max_length);
    while low < high {
        let middle = (low + high) / 2;
        if pack(&lengths, middle).len() <= fewest {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    pack(&lengths, low)
        .into_iter()
        .map(|(s, e)| atom_list[s..e].join(" "))
        .collect()
}

/// The graph's window: 510 phonemes (`MAX_PHONEME_LENGTH`).
pub const MAX_PHONEME_LENGTH: usize = 510;

/// [`split_phonemes_max`] at the graph's window.
#[must_use]
pub fn split_phonemes(phonemes: &str) -> Vec<String> {
    split_phonemes_max(phonemes, MAX_PHONEME_LENGTH)
}

/// `pause_after(phonemes, sentence, clause)` — seconds of silence a batch
/// ending with this text is followed by.
#[must_use]
pub fn pause_after(phonemes: &str, sentence: f64, clause: f64) -> f64 {
    let Some(mark) = phonemes.trim_end_matches(py_isspace).chars().last() else {
        return 0.0;
    };
    if SENTENCE_MARKS.contains(mark) {
        sentence
    } else if CLAUSE_MARKS.contains(mark) {
        clause
    } else {
        0.0
    }
}

/// `_prepare`'s batches: every batch but the last carries the pause its
/// final mark calls for; the last carries none.
#[must_use]
pub fn batches(phonemes: &str) -> Vec<(String, f64)> {
    let parts = split_phonemes(&normalize(phonemes));
    let n = parts.len();
    parts
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            let pause = if i + 1 < n {
                pause_after(&p, SENTENCE_PAUSE, CLAUSE_PAUSE)
            } else {
                0.0
            };
            (p, pause)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_input_is_one_batch() {
        assert_eq!(split_phonemes("ðə bˈɪld"), vec!["ðə bˈɪld".to_string()]);
        assert!(split_phonemes("   ").is_empty());
    }

    #[test]
    fn sentences_are_cut_before_clauses_and_words() {
        let got = split_phonemes_max("aaa bbb. ccc, ddd eee.", 12);
        assert_eq!(got, vec!["aaa bbb.", "ccc,", "ddd eee."]);
    }

    #[test]
    fn an_unbroken_run_is_sliced() {
        assert_eq!(split_phonemes_max("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn pauses_follow_the_final_mark() {
        assert_eq!(pause_after("a b. ", 0.25, 0.1), 0.25);
        assert_eq!(pause_after("a b,", 0.25, 0.1), 0.1);
        assert_eq!(pause_after("a b", 0.25, 0.1), 0.0);
        assert_eq!(pause_after("", 0.25, 0.1), 0.0);
    }

    #[test]
    fn the_last_batch_never_pauses() {
        let long = "ðə bˈɪld ɪz ɡɹˈin. ".repeat(60);
        let b = batches(&long);
        assert!(b.len() > 1);
        assert_eq!(b.last().unwrap().1, 0.0);
        assert!(b[..b.len() - 1].iter().all(|(_, p)| *p == SENTENCE_PAUSE));
        assert!(b
            .iter()
            .all(|(s, _)| s.chars().count() <= MAX_PHONEME_LENGTH));
    }

    #[test]
    fn normalize_collapses_newlines() {
        assert_eq!(normalize(" a\n\nb  c "), "a b c");
    }
}
