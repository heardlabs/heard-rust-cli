//! The voice shaping `Daemon._start_speech` applies ONCE to every spoken
//! line, in the Python's order:
//!
//! 1. [`register_apply`] — `register.apply` (tone: casual ↔ formal; Neutral is
//!    the identity);
//! 2. [`style_line`] — `style_lite.style_line`, template (`via=fastpath`)
//!    lines only;
//! 3. [`sanitize_spoken`] — `daemon._sanitize_spoken`: never read a file
//!    path verbatim, collapse it to its stem.
//!
//! Python's `re` has lookarounds the `regex` crate does not; each one used
//! here is hand-rolled and named where it happens.

use once_cell::sync::Lazy;
use regex::{Captures, Regex};
use serde_json::Value;

use crate::policy::{is_neutral_register, py_int};

// ---------------------------------------------------------------------------
// daemon._sanitize_spoken

static PATH_TOKEN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:[\w.-]+/)+([\w-]+)(\.[A-Za-z]\w{0,4})?").expect("path regex"));

/// `daemon._sanitize_spoken` — collapse any bare filesystem path to its
/// basename stem. A token only collapses with 2+ slashes OR a trailing
/// extension, so "and/or" and "TCP/IP" are left alone.
pub fn sanitize_spoken(text: &str) -> String {
    PATH_TOKEN
        .replace_all(text, |c: &Captures| {
            let tok = &c[0];
            if tok.matches('/').count() >= 2 || c.get(2).is_some() {
                c[1].to_string()
            } else {
                tok.to_string()
            }
        })
        .into_owned()
}

// ---------------------------------------------------------------------------
// style_lite.py

/// `style_lite.MIN_CHARS`.
pub const STYLE_MIN_CHARS: usize = 60;

static FILLERS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:just|simply|basically|actually|currently|really|essentially|now|then|in order to|go ahead and)\b\s*",
    )
    .expect("fillers regex")
});

/// `re.split(r"(?<=[.!?])\s+", text, maxsplit=1)[0]` on the stripped text.
fn first_sentence(text: &str) -> String {
    let t = text.trim();
    let mut prev: Option<char> = None;
    for (i, c) in t.char_indices() {
        if c.is_whitespace() && matches!(prev, Some('.' | '!' | '?')) {
            return t[..i].trim().to_string();
        }
        prev = Some(c);
    }
    t.to_string()
}

fn cap_words(text: &str, limit: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= limit {
        return text.to_string();
    }
    let joined = words[..limit].join(" ");
    let out = joined.trim_end_matches([',', ';', ':', '—', '-', ' ']);
    if out.ends_with(['.', '!', '?']) {
        out.to_string()
    } else {
        format!("{out}.")
    }
}

static MULTI_WS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s{2,}").expect("ws regex"));
static SPACE_PUNCT: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+([,.;:!?])").expect("sp regex"));

fn tidy(text: &str) -> String {
    let t = MULTI_WS.replace_all(text, " ");
    let t = t.trim();
    let t = SPACE_PUNCT.replace_all(t, "$1").into_owned();
    upper_first_if_lower(&t)
}

fn upper_first_if_lower(t: &str) -> String {
    let mut chars = t.chars();
    match chars.next() {
        Some(c) if c.is_lowercase() => c.to_uppercase().chain(chars).collect(),
        _ => t.to_string(),
    }
}

/// `style_lite.style_line(text, skill, kind=, tag=)`.
pub fn style_line(text: &str, skill: Option<&Value>, tag: &str) -> String {
    let body = text.trim();
    let name = match skill {
        None | Some(Value::Null) => "default".to_string(),
        Some(Value::String(s)) if s.is_empty() => "default".to_string(),
        Some(v) => crate::policy::py_str(v).trim().to_lowercase(),
    };
    if body.is_empty()
        || body.chars().count() < STYLE_MIN_CHARS
        || matches!(name.as_str(), "" | "default" | "verbose" | "custom")
    {
        return text.to_string();
    }
    if tag.to_lowercase() == "tool_question" {
        return text.to_string();
    }
    let limit = match name.as_str() {
        "adhd" => 12,
        "karpathy" => 20,
        _ => return text.to_string(),
    };
    let first = first_sentence(body);
    tidy(&cap_words(&FILLERS.replace_all(&first, ""), limit))
}

// ---------------------------------------------------------------------------
// register.py

const CONTRACT: &[(&str, &str)] = &[
    (r"\bit is\b", "it's"),
    (r"\bthat is\b", "that's"),
    (r"\bthere is\b", "there's"),
    (r"\bdo not\b", "don't"),
    (r"\bdoes not\b", "doesn't"),
    (r"\bdid not\b", "didn't"),
    (r"\bcannot\b", "can't"),
    (r"\bcould not\b", "couldn't"),
    (r"\bwould not\b", "wouldn't"),
    (r"\bshould not\b", "shouldn't"),
    (r"\bis not\b", "isn't"),
    (r"\bare not\b", "aren't"),
    (r"\bwas not\b", "wasn't"),
    (r"\bwere not\b", "weren't"),
    (r"\bhave not\b", "haven't"),
    (r"\bhas not\b", "hasn't"),
    (r"\bwill not\b", "won't"),
    (r"\bI am\b", "I'm"),
    (r"\bI have\b", "I've"),
    (r"\bI will\b", "I'll"),
    (r"\byou are\b", "you're"),
    (r"\byou have\b", "you've"),
    (r"\bwe are\b", "we're"),
    (r"\bwe have\b", "we've"),
    (r"\bthey are\b", "they're"),
    (r"\blet us\b", "let's"),
];

const EXPAND: &[(&str, &str)] = &[
    (r"\bit's\b", "it is"),
    (r"\bthat's\b", "that is"),
    (r"\bthere's\b", "there is"),
    (r"\bdon't\b", "do not"),
    (r"\bdoesn't\b", "does not"),
    (r"\bdidn't\b", "did not"),
    (r"\bcan't\b", "cannot"),
    (r"\bcouldn't\b", "could not"),
    (r"\bwouldn't\b", "would not"),
    (r"\bshouldn't\b", "should not"),
    (r"\bisn't\b", "is not"),
    (r"\baren't\b", "are not"),
    (r"\bwasn't\b", "was not"),
    (r"\bweren't\b", "were not"),
    (r"\bhaven't\b", "have not"),
    (r"\bhasn't\b", "has not"),
    (r"\bwon't\b", "will not"),
    (r"\bI'm\b", "I am"),
    (r"\bI've\b", "I have"),
    (r"\bI'll\b", "I will"),
    (r"\byou're\b", "you are"),
    (r"\byou've\b", "you have"),
    (r"\bwe're\b", "we are"),
    (r"\bwe've\b", "we have"),
    (r"\bthey're\b", "they are"),
    (r"\blet's\b", "let us"),
    (r"\bgonna\b", "going to"),
    (r"\bwanna\b", "want to"),
    (r"\bgotta\b", "have to"),
];

const LEXICON: &[(&str, &str)] = &[
    ("fix", "resolve"),
    ("fixed", "resolved"),
    ("run", "execute"),
    ("ran", "executed"),
    ("running", "executing"),
    ("done", "complete"),
    ("finished", "completed"),
    ("broke", "failed"),
    ("broken", "failing"),
    ("bug", "defect"),
    ("start", "begin"),
    ("started", "began"),
    ("starting", "beginning"),
    ("check", "verify"),
    ("checking", "verifying"),
    ("looking at", "examining"),
    ("kick off", "initiate"),
    ("sec", "moment"),
    ("okay", "very well"),
    ("yep", "yes"),
    ("nope", "no"),
];

const CASUAL_OPENERS: &[&str] = &["Okay — ", "Alright, ", "So, ", "Right — "];

static OPENER_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(?:okay|alright|so|right|well|now)\s*[—,-]?\s+").expect("opener regex")
});
static HEDGE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:basically|kind of|sort of|pretty much|just)\s+").expect("hedge regex")
});
static FIRST_WORD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z]+").expect("word regex"));

struct Swap {
    re: Regex,
    dst: &'static str,
}

fn compile(pairs: &'static [(&'static str, &'static str)], escape: bool) -> Vec<Swap> {
    pairs
        .iter()
        .map(|(src, dst)| {
            let pat = if escape {
                format!(r"(?i)\b{}\b", regex::escape(src))
            } else {
                format!("(?i){src}")
            };
            Swap {
                re: Regex::new(&pat).expect("register regex"),
                dst,
            }
        })
        .collect()
}

static CONTRACT_RE: Lazy<Vec<Swap>> = Lazy::new(|| compile(CONTRACT, false));
static EXPAND_RE: Lazy<Vec<Swap>> = Lazy::new(|| compile(EXPAND, false));

static LEXICON_FORMAL: Lazy<Vec<Swap>> = Lazy::new(|| {
    LEXICON
        .iter()
        .map(|(casual, polite)| Swap {
            re: Regex::new(&format!(r"(?i)\b{}\b", regex::escape(casual))).expect("lex"),
            dst: polite,
        })
        .collect()
});
static LEXICON_CASUAL: Lazy<Vec<Swap>> = Lazy::new(|| {
    LEXICON
        .iter()
        .map(|(casual, polite)| Swap {
            re: Regex::new(&format!(r"(?i)\b{}\b", regex::escape(polite))).expect("lex"),
            dst: casual,
        })
        .collect()
});

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_acronym(token: &str) -> bool {
    // Python: len > 1 and token.isupper() (has a cased char, all cased upper).
    token.chars().count() > 1
        && token.chars().any(char::is_alphabetic)
        && !token.chars().any(char::is_lowercase)
}

fn leading_acronym(text: &str) -> bool {
    match FIRST_WORD_RE.find(text.trim_start()) {
        Some(m) => is_acronym(m.as_str()),
        None => false,
    }
}

/// `_keep_case(found, replacement)`.
fn keep_case(found: &str, replacement: &str) -> String {
    if is_acronym(found) {
        return replacement.to_uppercase();
    }
    let f = found.chars().next();
    let r = replacement.chars().next();
    if let (Some(f), Some(r)) = (f, r) {
        if f.is_uppercase() && r.is_lowercase() {
            let mut rest = replacement.chars();
            rest.next();
            return r.to_uppercase().chain(rest).collect();
        }
    }
    replacement.to_string()
}

fn sub_all(text: &str, swaps: &[Swap]) -> String {
    let mut t = text.to_string();
    for s in swaps {
        t =
            s.re.replace_all(&t, |c: &Captures| keep_case(&c[0], s.dst))
                .into_owned();
    }
    t
}

static PROT_DQ: Lazy<Regex> = Lazy::new(|| Regex::new(r#"^"[^"]*""#).expect("dq"));
static PROT_BT: Lazy<Regex> = Lazy::new(|| Regex::new(r"^`[^`]*`").expect("bt"));
static PROT_PATH: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(?:~|/)[\w./-]+").expect("path"));
static PROT_IDENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\w+(?:[._-]\w+)+\b").expect("ident"));

/// One `_PROTECTED` match starting exactly at byte `i`, as its length.
///
/// `(\"[^\"]*\"|(?<!\w)'[^']*'(?!\w)|`[^`]*`|(?:~|/)[\w./-]+|\b\w+(?:[._-]\w+)+\b)`
/// with the lookarounds and the leading `\b` checked by hand against the
/// character before `i`.
fn protected_at(text: &str, i: usize) -> Option<usize> {
    let rest = &text[i..];
    let prev = text[..i].chars().next_back();
    if let Some(m) = PROT_DQ.find(rest) {
        return Some(m.end());
    }
    if rest.starts_with('\'') && !prev.is_some_and(is_word_char) {
        if let Some(close) = rest[1..].find('\'') {
            let end = 1 + close + 1;
            if !rest[end..].chars().next().is_some_and(is_word_char) {
                return Some(end);
            }
        }
    }
    if let Some(m) = PROT_BT.find(rest) {
        return Some(m.end());
    }
    if let Some(m) = PROT_PATH.find(rest) {
        return Some(m.end());
    }
    if !prev.is_some_and(is_word_char) {
        if let Some(m) = PROT_IDENT.find(rest) {
            return Some(m.end());
        }
    }
    None
}

/// `_PROTECTED.split(text)` then `fn` over the plain segments only.
fn apply_outside_protected(text: &str, f: impl Fn(&str) -> String) -> String {
    let mut out = String::new();
    let mut plain_start = 0;
    let mut i = 0;
    while i < text.len() {
        if let Some(len) = protected_at(text, i).filter(|l| *l > 0) {
            out.push_str(&f(&text[plain_start..i]));
            out.push_str(&text[i..i + len]);
            i += len;
            plain_start = i;
            continue;
        }
        i += text[i..].chars().next().map_or(1, char::len_utf8);
    }
    out.push_str(&f(&text[plain_start..]));
    out
}

fn casual_words(text: &str) -> String {
    sub_all(&sub_all(text, &CONTRACT_RE), &LEXICON_CASUAL)
}

fn formal_words(text: &str) -> String {
    let t = sub_all(text, &EXPAND_RE);
    let t = HEDGE_RE.replace_all(&t, "").into_owned();
    sub_all(&t, &LEXICON_FORMAL)
}

fn casual_opener(text: &str, allow_opener: bool) -> String {
    if !allow_opener {
        return text.to_string();
    }
    let first_alpha = text.chars().next().is_some_and(char::is_alphabetic);
    if text.split_whitespace().count() < 6 || OPENER_RE.is_match(text) || !first_alpha {
        return text.to_string();
    }
    let sum: u64 = text.chars().map(|c| u64::from(c as u32)).sum();
    let opener = CASUAL_OPENERS[(sum % CASUAL_OPENERS.len() as u64) as usize];
    if leading_acronym(text) {
        return format!("{opener}{text}");
    }
    let mut chars = text.chars();
    let first = chars
        .next()
        .map(|c| c.to_lowercase().collect::<String>())
        .unwrap_or_default();
    format!("{opener}{first}{}", chars.as_str())
}

fn formal_finish(text: &str) -> String {
    let lower_first = text.chars().next().is_some_and(char::is_lowercase);
    if lower_first && !leading_acronym(text) {
        return upper_first_if_lower(text);
    }
    text.to_string()
}

/// `register.apply(text, stop, kind=, tag=)` — Neutral / unset / unknown →
/// unchanged; questions keep their exact wording; protected spans (quotes,
/// backticks, paths, identifiers) are left alone.
///
/// The core's reading: no kind is verbatim. See [`register_apply_with`].
pub fn register_apply(text: &str, stop: Option<&Value>, kind: &str, tag: &str) -> String {
    register_apply_with::<&str>(text, stop, kind, tag, &[])
}

/// [`register_apply`], with `verbatim_kinds` — the kinds an extension speaks
/// as written ([`crate::Extension::verbatim_kinds`], lowercased). The casual
/// register never prepends an opener to a line of one of those kinds; every
/// other rule applies as usual.
pub fn register_apply_with<S: AsRef<str>>(
    text: &str,
    stop: Option<&Value>,
    kind: &str,
    tag: &str,
    verbatim_kinds: &[S],
) -> String {
    if is_neutral_register(stop) || text.trim().is_empty() {
        return text.to_string();
    }
    let Some(stop) = stop.and_then(py_int) else {
        return text.to_string();
    };
    if !(0..=2).contains(&stop) {
        return text.to_string();
    }
    if tag.to_lowercase() == "tool_question" {
        return text.to_string();
    }
    if stop < 1 {
        let out = apply_outside_protected(text, casual_words);
        let kind = kind.to_lowercase();
        let allow = !verbatim_kinds.iter().any(|k| k.as_ref() == kind);
        return casual_opener(&out, allow);
    }
    let stripped = OPENER_RE.replace(text, "").into_owned();
    formal_finish(&apply_outside_protected(&stripped, formal_words))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_collapses_to_its_stem() {
        assert_eq!(
            sanitize_spoken("in notes/release-checklist.csv, Sir."),
            "in release-checklist, Sir."
        );
        assert_eq!(sanitize_spoken("and/or TCP/IP"), "and/or TCP/IP");
    }

    #[test]
    fn neutral_register_is_identity() {
        assert_eq!(
            register_apply("It is done.", Some(&Value::from(1)), "", ""),
            "It is done."
        );
        assert_eq!(register_apply("It is done.", None, "", ""), "It is done.");
    }

    #[test]
    fn a_verbatim_kind_gets_no_casual_opener() {
        let text = "the build finished and every test in the suite passed";
        let casual = Some(Value::from(0));
        let dressed = register_apply(text, casual.as_ref(), "extension_answer", "");
        let plain = register_apply_with(
            text,
            casual.as_ref(),
            "Extension_Answer",
            "",
            &["extension_answer"],
        );
        assert_ne!(dressed, plain, "the core dresses an unknown kind");
        assert_eq!(plain, apply_outside_protected(text, casual_words));
        // Any other kind is untouched by the verbatim list.
        assert_eq!(
            register_apply_with(text, casual.as_ref(), "final", "", &["extension_answer"]),
            register_apply(text, casual.as_ref(), "final", "")
        );
    }

    #[test]
    fn formal_expands_and_casual_contracts() {
        assert_eq!(
            register_apply("okay, it's done", Some(&Value::from(2)), "", ""),
            "It is complete"
        );
        assert_eq!(
            register_apply("It is done.", Some(&Value::from(0)), "", ""),
            "It's done."
        );
    }
}
