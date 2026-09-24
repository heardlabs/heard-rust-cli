//! `yaml/resolver.py`: PyYAML's YAML 1.1 implicit resolvers.
//!
//! Each regex from `Resolver.add_implicit_resolver` is matched by hand. Two
//! Python details matter and are kept:
//!
//! * the resolvers are tried in REGISTRATION order among those registered for
//!   the value's first character (bool, float, int, merge, null, timestamp,
//!   value, yaml), and
//! * the patterns end in `$`, which in Python also matches just before ONE
//!   trailing `"\n"` — so `"yes\n"` resolves as a bool.
//!
//! The same function decides, on the way out, whether a string can be written
//! plain: PyYAML's serializer asks "would this text resolve back as a str?",
//! and quotes it when the answer is no.

pub(crate) const STR: &str = "tag:yaml.org,2002:str";
pub(crate) const SEQ: &str = "tag:yaml.org,2002:seq";
pub(crate) const MAP: &str = "tag:yaml.org,2002:map";
pub(crate) const BOOL: &str = "tag:yaml.org,2002:bool";
pub(crate) const INT: &str = "tag:yaml.org,2002:int";
pub(crate) const FLOAT: &str = "tag:yaml.org,2002:float";
pub(crate) const NULL: &str = "tag:yaml.org,2002:null";
pub(crate) const TIMESTAMP: &str = "tag:yaml.org,2002:timestamp";
pub(crate) const MERGE: &str = "tag:yaml.org,2002:merge";
pub(crate) const VALUE: &str = "tag:yaml.org,2002:value";
pub(crate) const YAML: &str = "tag:yaml.org,2002:yaml";
pub(crate) const BINARY: &str = "tag:yaml.org,2002:binary";
pub(crate) const SET: &str = "tag:yaml.org,2002:set";
pub(crate) const OMAP: &str = "tag:yaml.org,2002:omap";
pub(crate) const PAIRS: &str = "tag:yaml.org,2002:pairs";

/// `Resolver.resolve(ScalarNode, value, implicit)`.
pub(crate) fn resolve_scalar(value: &str, plain_implicit: bool) -> &'static str {
    if plain_implicit {
        if let Some(tag) = implicit_tag(value) {
            return tag;
        }
    }
    STR
}

/// The tag an implicit resolver assigns to plain `value`, if any.
pub(crate) fn implicit_tag(value: &str) -> Option<&'static str> {
    let first = value.chars().next();
    type Matcher = fn(&str) -> bool;
    let candidates: &[(&'static str, Matcher)] = match first {
        None => &[(NULL, is_null)],
        Some(c) => match c {
            'y' | 'Y' | 'n' | 'N' | 't' | 'T' | 'f' | 'F' | 'o' | 'O' => {
                if c == 'n' || c == 'N' {
                    &[(BOOL, is_bool), (NULL, is_null)]
                } else {
                    &[(BOOL, is_bool)]
                }
            }
            '-' | '+' => &[(FLOAT, is_float), (INT, is_int)],
            '0'..='9' => &[(FLOAT, is_float), (INT, is_int), (TIMESTAMP, is_timestamp)],
            '.' => &[(FLOAT, is_float)],
            '<' => &[(MERGE, is_merge)],
            '~' => &[(NULL, is_null)],
            '=' => &[(VALUE, is_value)],
            '!' | '&' | '*' => &[(YAML, is_yaml)],
            _ => &[],
        },
    };
    candidates
        .iter()
        .find(|(_, m)| dollar(value, *m))
        .map(|(tag, _)| *tag)
}

/// Python's `^…$`: a full match, or a full match of everything but one final
/// `"\n"`.
pub(crate) fn dollar(value: &str, m: fn(&str) -> bool) -> bool {
    m(value) || value.strip_suffix('\n').is_some_and(m)
}

fn is_bool(s: &str) -> bool {
    matches!(
        s,
        "yes"
            | "Yes"
            | "YES"
            | "no"
            | "No"
            | "NO"
            | "true"
            | "True"
            | "TRUE"
            | "false"
            | "False"
            | "FALSE"
            | "on"
            | "On"
            | "ON"
            | "off"
            | "Off"
            | "OFF"
    )
}

fn is_null(s: &str) -> bool {
    matches!(s, "~" | "null" | "Null" | "NULL" | "")
}

fn is_merge(s: &str) -> bool {
    s == "<<"
}

fn is_value(s: &str) -> bool {
    s == "="
}

fn is_yaml(s: &str) -> bool {
    matches!(s, "!" | "&" | "*")
}

/// A cursor over ASCII bytes; every pattern here is ASCII-only, so a
/// non-ASCII byte simply fails to match.
struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cur<'a> {
    fn new(s: &'a str) -> Self {
        Cur {
            b: s.as_bytes(),
            i: 0,
        }
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn eat_sign(&mut self) {
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.i += 1;
        }
    }
    /// Consume `[set]*`, returning how many.
    fn run(&mut self, pred: impl Fn(u8) -> bool) -> usize {
        let start = self.i;
        while self.peek().is_some_and(&pred) {
            self.i += 1;
        }
        self.i - start
    }
    fn done(&self) -> bool {
        self.i == self.b.len()
    }
    fn rest(&self) -> &'a [u8] {
        &self.b[self.i..]
    }
}

fn digit(c: u8) -> bool {
    c.is_ascii_digit()
}
fn digit_(c: u8) -> bool {
    c.is_ascii_digit() || c == b'_'
}

/// `(?:[eE][-+][0-9]+)?` then end.
fn opt_exponent_then_end(c: &mut Cur) -> bool {
    if matches!(c.peek(), Some(b'e' | b'E')) {
        c.i += 1;
        if !matches!(c.peek(), Some(b'-' | b'+')) {
            return false;
        }
        c.i += 1;
        if c.run(digit) == 0 {
            return false;
        }
    }
    c.done()
}

/// `(?::[0-5]?[0-9])+` — each segment is a maximal digit run of length 1, or
/// of length 2 whose first digit is 0-5 (what follows a segment is always a
/// non-digit, so the regex can only match a whole run).
fn sexagesimal_segments(c: &mut Cur) -> bool {
    let mut n = 0;
    while c.peek() == Some(b':') {
        c.i += 1;
        let start = c.i;
        let len = c.run(digit);
        match len {
            1 => {}
            2 if c.b[start] <= b'5' => {}
            _ => return false,
        }
        n += 1;
    }
    n > 0
}

/// The float resolver:
///
/// ```text
/// [-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?
/// |\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?
/// |[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*
/// |[-+]?\.(?:inf|Inf|INF)
/// |\.(?:nan|NaN|NAN)
/// ```
pub(crate) fn is_float(s: &str) -> bool {
    // Alternative 1.
    {
        let mut c = Cur::new(s);
        c.eat_sign();
        if c.peek().is_some_and(digit) {
            c.run(digit_);
            if c.eat(b'.') {
                c.run(digit_);
                if opt_exponent_then_end(&mut c) {
                    return true;
                }
            }
        }
    }
    // Alternative 2.
    {
        let mut c = Cur::new(s);
        if c.eat(b'.') && c.peek().is_some_and(digit) {
            c.run(digit_);
            if opt_exponent_then_end(&mut c) {
                return true;
            }
        }
    }
    // Alternative 3.
    {
        let mut c = Cur::new(s);
        c.eat_sign();
        if c.peek().is_some_and(digit) {
            c.run(digit_);
            if sexagesimal_segments(&mut c) && c.eat(b'.') {
                c.run(digit_);
                if c.done() {
                    return true;
                }
            }
        }
    }
    // Alternatives 4 and 5.
    {
        let mut c = Cur::new(s);
        c.eat_sign();
        if c.eat(b'.') && matches!(c.rest(), b"inf" | b"Inf" | b"INF") {
            return true;
        }
    }
    matches!(s, ".nan" | ".NaN" | ".NAN")
}

/// The int resolver:
///
/// ```text
/// [-+]?0b[0-1_]+
/// |[-+]?0[0-7_]+
/// |[-+]?(?:0|[1-9][0-9_]*)
/// |[-+]?0x[0-9a-fA-F_]+
/// |[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+
/// ```
pub(crate) fn is_int(s: &str) -> bool {
    let mut c = Cur::new(s);
    c.eat_sign();
    let body = c.rest();
    let full = |pred: fn(u8) -> bool, from: usize| {
        body.len() > from && body[from..].iter().all(|b| pred(*b))
    };
    if body.starts_with(b"0b") && full(|b| b == b'0' || b == b'1' || b == b'_', 2) {
        return true;
    }
    if body.starts_with(b"0") && full(|b| (b'0'..=b'7').contains(&b) || b == b'_', 1) {
        return true;
    }
    if body == b"0" {
        return true;
    }
    if body.first().is_some_and(|b| (b'1'..=b'9').contains(b)) && body.iter().all(|b| digit_(*b)) {
        return true;
    }
    if body.starts_with(b"0x") && full(|b| b.is_ascii_hexdigit() || b == b'_', 2) {
        return true;
    }
    if body.first().is_some_and(|b| (b'1'..=b'9').contains(b)) {
        let mut c2 = Cur { b: body, i: 0 };
        c2.run(digit_);
        if sexagesimal_segments(&mut c2) && c2.done() {
            return true;
        }
    }
    false
}

/// The timestamp resolver:
///
/// ```text
/// [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]
/// |[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?
///  (?:[Tt]|[ \t]+)[0-9][0-9]?:[0-9][0-9]:[0-9][0-9](?:\.[0-9]*)?
///  (?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?
/// ```
pub(crate) fn is_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() == 10
        && b[..4].iter().all(|c| digit(*c))
        && b[4] == b'-'
        && b[5..7].iter().all(|c| digit(*c))
        && b[7] == b'-'
        && b[8..10].iter().all(|c| digit(*c))
    {
        return true;
    }
    parse_timestamp(s, true).is_some()
}

/// The pieces of a timestamp, as `SafeConstructor.timestamp_regexp` groups
/// them. With `require_time` it is the resolver's second alternative (a time
/// is mandatory); without, it is the constructor's regex (time optional).
#[derive(Debug, Default, Clone)]
pub(crate) struct TimestampParts<'a> {
    pub year: &'a str,
    pub month: &'a str,
    pub day: &'a str,
    pub time: Option<TimeParts<'a>>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct TimeParts<'a> {
    pub hour: &'a str,
    pub minute: &'a str,
    pub second: &'a str,
    pub fraction: Option<&'a str>,
    /// `None` = no tz; `Some(None)` = `Z`; `Some(Some((sign, hour, minute)))`.
    pub tz: Option<Option<(u8, &'a str, Option<&'a str>)>>,
}

fn sub(s: &str, from: usize, to: usize) -> &str {
    &s[from..to]
}

/// Match `s` (with the `$`-before-final-newline allowance) against the
/// timestamp grammar.
pub(crate) fn parse_timestamp(s: &str, require_time: bool) -> Option<TimestampParts<'_>> {
    parse_timestamp_exact(s, require_time).or_else(|| {
        s.strip_suffix('\n')
            .and_then(|t| parse_timestamp_exact(t, require_time))
    })
}

fn parse_timestamp_exact(s: &str, require_time: bool) -> Option<TimestampParts<'_>> {
    let mut c = Cur::new(s);
    let y0 = c.i;
    if c.run(digit) < 4 {
        return None;
    }
    // `[0-9]{4}` is exactly four digits followed by '-'.
    if c.i - y0 != 4 {
        return None;
    }
    let year = sub(s, y0, c.i);
    if !c.eat(b'-') {
        return None;
    }
    let m0 = c.i;
    let ml = c.run(digit);
    if !(1..=2).contains(&ml) {
        return None;
    }
    let month = sub(s, m0, c.i);
    if !c.eat(b'-') {
        return None;
    }
    let d0 = c.i;
    let dl = c.run(digit);
    if !(1..=2).contains(&dl) {
        return None;
    }
    let day = sub(s, d0, c.i);
    let mut parts = TimestampParts {
        year,
        month,
        day,
        time: None,
    };
    if c.done() {
        return if require_time { None } else { Some(parts) };
    }
    // (?:[Tt]|[ \t]+)
    if !(c.eat(b'T') || c.eat(b't')) && c.run(|b| b == b' ' || b == b'\t') == 0 {
        return None;
    }
    let h0 = c.i;
    let hl = c.run(digit);
    if !(1..=2).contains(&hl) {
        return None;
    }
    let hour = sub(s, h0, c.i);
    if !c.eat(b':') {
        return None;
    }
    let mi0 = c.i;
    if c.run(digit) != 2 {
        return None;
    }
    let minute = sub(s, mi0, c.i);
    if !c.eat(b':') {
        return None;
    }
    let s0 = c.i;
    if c.run(digit) != 2 {
        return None;
    }
    let second = sub(s, s0, c.i);
    let mut time = TimeParts {
        hour,
        minute,
        second,
        fraction: None,
        tz: None,
    };
    if c.eat(b'.') {
        let f0 = c.i;
        c.run(digit);
        time.fraction = Some(sub(s, f0, c.i));
    }
    if !c.done() {
        c.run(|b| b == b' ' || b == b'\t');
        if c.eat(b'Z') {
            time.tz = Some(None);
        } else if matches!(c.peek(), Some(b'-' | b'+')) {
            let sign = c.peek().unwrap_or(b'+');
            c.i += 1;
            let th0 = c.i;
            let thl = c.run(digit);
            if !(1..=2).contains(&thl) {
                return None;
            }
            let th = sub(s, th0, c.i);
            let mut tm = None;
            if c.eat(b':') {
                let tm0 = c.i;
                if c.run(digit) != 2 {
                    return None;
                }
                tm = Some(sub(s, tm0, c.i));
            }
            time.tz = Some(Some((sign, th, tm)));
        } else {
            return None;
        }
        if !c.done() {
            return None;
        }
    }
    parts.time = Some(time);
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_brief_examples() {
        assert_eq!(implicit_tag("yes"), Some(BOOL));
        assert_eq!(implicit_tag("on"), Some(BOOL));
        assert_eq!(implicit_tag("1e+16"), None, "PyYAML's float needs a dot");
        assert_eq!(implicit_tag("1.0e+16"), Some(FLOAT));
        assert_eq!(implicit_tag("0x1A"), Some(INT));
        assert_eq!(implicit_tag("1_000"), Some(INT));
        assert_eq!(implicit_tag("12:30"), Some(INT));
        assert_eq!(implicit_tag("12:30.5"), Some(FLOAT));
        assert_eq!(implicit_tag("2026-09-23"), Some(TIMESTAMP));
        assert_eq!(implicit_tag("2026-9-23"), None);
        assert_eq!(implicit_tag("2026-9-23 1:02:03"), Some(TIMESTAMP));
        assert_eq!(implicit_tag("~"), Some(NULL));
        assert_eq!(implicit_tag(""), Some(NULL));
        assert_eq!(implicit_tag("null"), Some(NULL));
        assert_eq!(implicit_tag(".inf"), Some(FLOAT));
        assert_eq!(implicit_tag("-.INF"), Some(FLOAT));
        assert_eq!(implicit_tag("+.nan"), None);
        assert_eq!(implicit_tag("08"), None);
        assert_eq!(implicit_tag("07"), Some(INT));
        assert_eq!(implicit_tag("yes\n"), Some(BOOL));
        assert_eq!(implicit_tag("<<"), Some(MERGE));
        assert_eq!(implicit_tag("="), Some(VALUE));
        assert_eq!(implicit_tag("1:60"), None);
        assert_eq!(implicit_tag("1:5"), Some(INT));
    }
}
