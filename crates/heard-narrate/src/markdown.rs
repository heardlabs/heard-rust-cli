//! Strip markdown-flavored assistant output into plain spoken text.
//!
//! Port of `engine/heard/markdown.py`. The stripper runs on every assistant
//! text block before TTS, so any rough edge shows up immediately as weird
//! speech — pipes read aloud as "pipe", `> blockquote` lines spoken as
//! "greater than" (`engine/tests/test_markdown.py`).
//!
//! The order of the passes is load-bearing and is kept exactly as Python has
//! it: code blocks go first, because pipes and asterisks *inside* code are not
//! markdown and must not be re-read by the emphasis or table passes.
//!
//! ## Where this deliberately does not use a regex
//!
//! Python's italic rule is `(?<!\*)\*([^*\n]+)\*` — a negative LOOKBEHIND,
//! which the `regex` crate does not support (it is a finite-automata engine, by
//! design). [`strip_italic`] hand-rolls exactly that rule instead: a `*` that
//! is not itself preceded by a `*`, a non-empty run with no `*` and no newline
//! in it, and a closing `*` — scanning forward from the failed position the way
//! a backtracking engine retries, so the match set is identical.
//!
//! ## Known, bounded divergence from Python's `re`
//!
//! Python's `\s` on `str` additionally matches the C0 file/group/record/unit
//! separators `\x1c`–`\x1f` (they are `str.isspace()`), which are NOT in
//! Unicode's `White_Space` property and so are not matched by Rust's `\s` or
//! [`char::is_whitespace`]. Nothing in the corpus — or in real assistant
//! output — contains them. Every other whitespace character (including
//! `\u{a0}`, `\u{85}`, `\u{2028}`) behaves identically in both.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

// Pre-compiled because the daemon runs this on every event.
static FENCED_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)```.*?```").unwrap());
static INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());
static INDENTED_CODE_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)(?:^|\n)((?:[ \t]{4,}[^\n]*\n?)+)").unwrap());
static IMG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"!\[[^\]]*\]\([^)]+\)").unwrap());
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(https?://[^)]+\)").unwrap());
static BARE_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://\S+").unwrap());
static BOLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*([^*]+)\*\*").unwrap());
static STRIKETHROUGH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"~~([^~\n]+)~~").unwrap());
static HEADER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^#{1,6}\s+").unwrap());
static BULLET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*[-*+]\s+").unwrap());
static NUMBERED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*\d+\.\s+").unwrap());
static BLOCKQUOTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*>+\s?").unwrap());
/// A table separator row: pipes + dashes (and optional `:`) only. Drop entirely.
static TABLE_SEP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*\|?[\s\-:|]+\|[\s\-:|]+\s*$").unwrap());
/// Table cell delimiter — turn pipes into commas so cells are read as a list,
/// not "pipe pipe pipe".
static TABLE_PIPE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\|\s*").unwrap());
/// Em-dash → comma for natural TTS pauses.
static EM_DASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*—\s*").unwrap());

const CODE_OMITTED: &str = " code block omitted ";

/// Feed one pass' output into the next without allocating when the pass did
/// nothing. A borrowed result means "unchanged", so the previous buffer (which
/// may itself be borrowed from the caller's `&str`) is kept as-is.
fn pipe<'a>(text: Cow<'a, str>, step: fn(&str) -> Cow<'_, str>) -> Cow<'a, str> {
    match step(&text) {
        Cow::Borrowed(_) => text,
        Cow::Owned(owned) => Cow::Owned(owned),
    }
}

/// Strip markdown down to what should be spoken.
///
/// Borrowed straight through when the text contains no markdown at all — the
/// common case for a short assistant line, and the reason the whole pipeline is
/// `Cow<str>` rather than `String`.
pub fn strip(text: &str) -> Cow<'_, str> {
    // Code blocks first — pipes / asterisks inside code aren't markdown.
    let out = pipe(Cow::Borrowed(text), |t| {
        FENCED_CODE.replace_all(t, CODE_OMITTED)
    });
    let out = pipe(out, |t| INDENTED_CODE_BLOCK.replace_all(t, CODE_OMITTED));
    let out = pipe(out, |t| INLINE_CODE.replace_all(t, "${1}"));
    // Links + images.
    let out = pipe(out, |t| IMG.replace_all(t, ""));
    let out = pipe(out, |t| LINK.replace_all(t, "${1}"));
    let out = pipe(out, |t| BARE_URL.replace_all(t, " a link "));
    // Inline emphasis.
    let out = pipe(out, |t| BOLD.replace_all(t, "${1}"));
    let out = pipe(out, strip_italic);
    let out = pipe(out, |t| STRIKETHROUGH.replace_all(t, "${1}"));
    // Block-level prefixes.
    let out = pipe(out, |t| HEADER.replace_all(t, ""));
    let out = pipe(out, |t| BULLET.replace_all(t, ""));
    let out = pipe(out, |t| NUMBERED.replace_all(t, ""));
    let out = pipe(out, |t| BLOCKQUOTE.replace_all(t, ""));
    // Tables: drop the alignment row, then turn pipe delimiters into commas so
    // cells read as a list.
    let out = pipe(out, |t| TABLE_SEP.replace_all(t, ""));
    let out = pipe(out, |t| TABLE_PIPE.replace_all(t, ", "));
    // Em-dash → comma for natural TTS pauses. Eat the surrounding space so we
    // don't end up with "one , two" (space-before-comma).
    let out = pipe(out, |t| EM_DASH.replace_all(t, ", "));
    let out = pipe(out, collapse_whitespace);
    match out {
        Cow::Borrowed(s) => Cow::Borrowed(trim_spaces_and_commas(s)),
        Cow::Owned(s) => {
            let trimmed = trim_spaces_and_commas(&s);
            if trimmed.len() == s.len() {
                Cow::Owned(s)
            } else {
                Cow::Owned(trimmed.to_owned())
            }
        }
    }
}

/// Python's `text.strip(" ,")`.
fn trim_spaces_and_commas(text: &str) -> &str {
    text.trim_matches(|c| c == ' ' || c == ',')
}

/// Python's `re.sub(r"\s+", " ", text)`, but borrowing when the text is already
/// normalised (no run of whitespace that isn't exactly one space).
fn collapse_whitespace(text: &str) -> Cow<'_, str> {
    let needs = {
        let mut needs = false;
        let mut run = 0usize;
        for c in text.chars() {
            if c.is_whitespace() {
                run += 1;
                if run > 1 || c != ' ' {
                    needs = true;
                    break;
                }
            } else {
                run = 0;
            }
        }
        needs
    };
    if !needs {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut in_ws = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    Cow::Owned(out)
}

/// Python's `_ITALIC = re.compile(r"(?<!\*)\*([^*\n]+)\*")`, hand-rolled
/// because the `regex` crate has no lookbehind.
///
/// `*`, `\n` and the ASCII text around them are all single bytes in UTF-8, so
/// this scans bytes and slices on boundaries it knows are safe.
fn strip_italic(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0usize; // everything before this is already in `out`
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'*' {
            i += 1;
            continue;
        }
        // The lookbehind: a `*` preceded by a `*` can never open an italic.
        if i > 0 && bytes[i - 1] == b'*' {
            i += 1;
            continue;
        }
        // `[^*\n]+` is greedy but cannot cross a `*` or a newline, so the run
        // it takes is fixed: everything up to the next one of those.
        let mut j = i + 1;
        while j < bytes.len() && bytes[j] != b'*' && bytes[j] != b'\n' {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b'*' && j > i + 1 {
            let buf = out.get_or_insert_with(|| String::with_capacity(text.len()));
            buf.push_str(&text[copied..i]);
            buf.push_str(&text[i + 1..j]);
            copied = j + 1;
            i = j + 1;
        } else {
            // No match here; a backtracking engine would retry one position on.
            i += 1;
        }
    }
    match out {
        Some(mut buf) => {
            buf.push_str(&text[copied..]);
            Cow::Owned(buf)
        }
        None => Cow::Borrowed(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_borrowed_not_reallocated() {
        assert!(matches!(strip("Nothing to strip here."), Cow::Borrowed(_)));
    }

    #[test]
    fn the_python_tests_hold() {
        // engine/tests/test_markdown.py, assertion for assertion.
        assert!(
            strip("Here is some code:\n```py\ndef x(): pass\n```\nand more")
                .contains("code block omitted")
        );
        assert_eq!(strip("Use `os.path` for paths."), "Use os.path for paths.");
        assert!(!strip("> Quoted line one\n> Quoted line two").contains('>'));
        assert_eq!(
            strip("This is ~~deprecated~~ now."),
            "This is deprecated now."
        );
        assert_eq!(strip("# Big\n## Smaller\nbody"), "Big Smaller body");
        assert_eq!(
            strip("Read [the docs](https://x.io/docs) please."),
            "Read the docs please."
        );
        assert_eq!(
            strip("Step one — then step two."),
            "Step one, then step two."
        );
        assert_eq!(
            strip("- first\n- second\n1. one\n2. two"),
            "first second one two"
        );
    }

    #[test]
    fn italic_lookbehind_is_honoured() {
        assert_eq!(
            strip("Hello **world** and *italics*"),
            "Hello world and italics"
        );
        assert_eq!(strip("***both***"), "both");
        assert_eq!(strip("*unclosed italic"), "*unclosed italic");
        assert_eq!(strip("*start\nend*"), "*start end*");
    }
}
