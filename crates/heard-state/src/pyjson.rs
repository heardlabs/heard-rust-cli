//! Order-preserving JSON, spelled the way CPython's `json` module spells it.
//!
//! `history.py` writes `json.dumps(record, ensure_ascii=False) + "\n"` and the
//! port has to be byte-for-byte identical, which rules out `serde_json`:
//!
//! * CPython's default separators are `", "` and `": "`. `serde_json`'s
//!   compact form has no spaces, so every byte of every line would differ.
//! * `ensure_ascii=False` emits real UTF-8. This matters: an em-dash in agent prose is routine and `—` in the log
//!   would be a regression, not a formatting detail.
//! * A record's key order is its Python insertion order (`append` copies the
//!   dict and then `setdefault("ts", …)`, so a caller-supplied `ts` stays where
//!   the caller put it and an implicit one lands last). `serde_json::Value`
//!   sorts object keys, which would silently reorder every record.
//!
//! So [`PyValue`] keeps a dict as an ordered `Vec<(String, PyValue)>`, and
//! [`PyValue::dumps`] and [`PyValue::loads`] are the two halves of the round
//! trip. Nothing here is a general-purpose JSON library: it is exactly the
//! subset the history log and the spoken-hash file put on disk.

use std::fmt::Write as _;

/// A JSON value that remembers the order its keys were inserted in.
#[derive(Debug, Clone, PartialEq)]
pub enum PyValue {
    Null,
    Bool(bool),
    /// Whole numbers stay integral — CPython prints `7`, never `7.0`.
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<PyValue>),
    Dict(Vec<(String, PyValue)>),
}

impl PyValue {
    /// Look a key up in a dict. First match wins, like CPython.
    pub fn get(&self, key: &str) -> Option<&PyValue> {
        match self {
            PyValue::Dict(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Set (or append) a key without disturbing the order of the others —
    /// the shape of `dict.__setitem__`.
    pub fn set(&mut self, key: &str, value: PyValue) {
        if let PyValue::Dict(pairs) = self {
            if let Some(slot) = pairs.iter_mut().find(|(k, _)| k == key) {
                slot.1 = value;
            } else {
                pairs.push((key.to_string(), value));
            }
        }
    }

    /// `dict.setdefault` — insert only when the key is absent.
    pub fn set_default(&mut self, key: &str, value: PyValue) {
        if self.get(key).is_none() {
            self.set(key, value);
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            PyValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[PyValue]> {
        match self {
            PyValue::List(items) => Some(items),
            _ => None,
        }
    }

    /// `json.dumps(value, ensure_ascii=False)`.
    pub fn dumps(&self) -> String {
        let mut out = String::new();
        self.write_into(&mut out);
        out
    }

    fn write_into(&self, out: &mut String) {
        match self {
            PyValue::Null => out.push_str("null"),
            PyValue::Bool(true) => out.push_str("true"),
            PyValue::Bool(false) => out.push_str("false"),
            PyValue::Int(n) => {
                let _ = write!(out, "{n}");
            }
            PyValue::Float(f) => out.push_str(&format_float(*f)),
            PyValue::Str(s) => write_string(s, out),
            PyValue::List(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    item.write_into(out);
                }
                out.push(']');
            }
            PyValue::Dict(pairs) => {
                out.push('{');
                for (i, (key, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_string(key, out);
                    out.push_str(": ");
                    value.write_into(out);
                }
                out.push('}');
            }
        }
    }

    /// `json.loads`, keeping object key order. Returns `None` on anything the
    /// history log would never have written — the callers all treat a parse
    /// failure as "skip this line", exactly as the Python `except Exception:
    /// continue` does.
    pub fn loads(text: &str) -> Option<PyValue> {
        let mut parser = Parser {
            chars: text.as_bytes(),
            pos: 0,
            src: text,
        };
        parser.skip_ws();
        let value = parser.value()?;
        parser.skip_ws();
        if parser.pos == parser.chars.len() {
            Some(value)
        } else {
            None
        }
    }
}

/// CPython's `float.__repr__`: the shortest string that round-trips, always
/// with a decimal point or an exponent so it can't be read back as an int.
fn format_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    // Rust's `{:?}` for f64 is also shortest-round-trip and also keeps a
    // trailing `.0`, so the two agree over the range a history record holds.
    format!("{f:?}")
}

/// CPython's `json.encoder.py_encode_basestring` with `ensure_ascii=False`:
/// escape only `"`, `\` and the C0 control characters. Notably NOT `/`, and
/// NOT anything non-ASCII.
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    chars: &'a [u8],
    pos: usize,
    src: &'a str,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.chars.len() {
            match self.chars[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.chars.get(self.pos).copied()
    }

    fn eat(&mut self, literal: &str) -> bool {
        if self.src[self.pos..].starts_with(literal) {
            self.pos += literal.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Option<PyValue> {
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string().map(PyValue::Str),
            b't' => self.eat("true").then_some(PyValue::Bool(true)),
            b'f' => self.eat("false").then_some(PyValue::Bool(false)),
            b'n' => self.eat("null").then_some(PyValue::Null),
            _ => self.number(),
        }
    }

    fn object(&mut self) -> Option<PyValue> {
        self.pos += 1; // '{'
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek()? == b'}' {
            self.pos += 1;
            return Some(PyValue::Dict(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if self.peek()? != b':' {
                return None;
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.value()?;
            // CPython keeps the LAST value for a repeated key but the FIRST
            // position. Vanishingly unlikely in this log; mirrored anyway.
            if let Some(slot) = pairs
                .iter_mut()
                .find(|(k, _): &&mut (String, PyValue)| *k == key)
            {
                slot.1 = value;
            } else {
                pairs.push((key, value));
            }
            self.skip_ws();
            match self.peek()? {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    return Some(PyValue::Dict(pairs));
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self) -> Option<PyValue> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek()? == b']' {
            self.pos += 1;
            return Some(PyValue::List(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek()? {
                b',' => self.pos += 1,
                b']' => {
                    self.pos += 1;
                    return Some(PyValue::List(items));
                }
                _ => return None,
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        if self.peek()? != b'"' {
            return None;
        }
        self.pos += 1;
        let mut out = String::new();
        loop {
            let start = self.pos;
            // Fast path: copy the run up to the next escape or closing quote.
            while self.pos < self.chars.len()
                && self.chars[self.pos] != b'"'
                && self.chars[self.pos] != b'\\'
            {
                self.pos += 1;
            }
            out.push_str(self.src.get(start..self.pos)?);
            match self.peek()? {
                b'"' => {
                    self.pos += 1;
                    return Some(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let esc = self.peek()?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
    }

    fn unicode_escape(&mut self) -> Option<char> {
        let first = self.hex4()?;
        if (0xD800..0xDC00).contains(&first) {
            // Surrogate pair — CPython's decoder joins them.
            if !self.eat("\\u") {
                return None;
            }
            let second = self.hex4()?;
            if !(0xDC00..0xE000).contains(&second) {
                return None;
            }
            let code = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
            return char::from_u32(code);
        }
        char::from_u32(first)
    }

    fn hex4(&mut self) -> Option<u32> {
        let slice = self.src.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        u32::from_str_radix(slice, 16).ok()
    }

    fn number(&mut self) -> Option<PyValue> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let mut is_float = false;
        while let Some(c) = self.peek() {
            match c {
                b'0'..=b'9' => self.pos += 1,
                b'.' | b'e' | b'E' | b'+' | b'-' => {
                    is_float = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let text = self.src.get(start..self.pos)?;
        if text.is_empty() {
            return None;
        }
        if is_float {
            text.parse::<f64>().ok().map(PyValue::Float)
        } else {
            text.parse::<i64>().ok().map(PyValue::Int)
        }
    }
}

/// Read one fixture value. Plain JSON — no guessing.
///
/// An earlier draft tried to recognise a dict by its shape (`[[key, value],
/// …]`) at every level, and got it wrong the first time it met a record whose
/// value genuinely *was* a list of two-element lists. A corpus that needs a
/// heuristic to read is a corpus that can encode two different things the same
/// way, so the marker is now structural and lives at exactly one level: a
/// history record is a pair list, everything inside it is ordinary JSON.
///
/// The consequence is that the corpus holds no *nested* ordered dict, because
/// nothing could express one unambiguously. Nothing in `history.jsonl` writes
/// one today; the round trip for nested objects is covered by this module's own
/// tests instead.
pub fn value_from_fixture(value: &serde_json::Value) -> PyValue {
    match value {
        serde_json::Value::Null => PyValue::Null,
        serde_json::Value::Bool(b) => PyValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                PyValue::Int(i)
            } else {
                PyValue::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => PyValue::Str(s.clone()),
        serde_json::Value::Array(items) => {
            PyValue::List(items.iter().map(value_from_fixture).collect())
        }
        serde_json::Value::Object(map) => PyValue::Dict(
            map.iter()
                .map(|(k, v)| (k.clone(), value_from_fixture(v)))
                .collect(),
        ),
    }
}

/// Read a history record out of a fixture, where it is stored as an ordered
/// `[[key, value], …]` list.
///
/// The corpus cannot use a plain JSON object here: a `serde_json::Value` sorts
/// object keys on the way in, which would destroy exactly the property the
/// history fixtures exist to pin. Pairs survive any JSON reader.
///
/// Returns `None` when the value is not a pair list, so a malformed fixture
/// fails loudly rather than silently reading as an empty record.
pub fn record_from_fixture(value: &serde_json::Value) -> Option<Vec<(String, PyValue)>> {
    let items = value.as_array()?;
    items
        .iter()
        .map(|item| {
            let pair = item.as_array()?;
            if pair.len() != 2 {
                return None;
            }
            Some((pair[0].as_str()?.to_string(), value_from_fixture(&pair[1])))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumps_matches_cpython_spacing() {
        let value = PyValue::Dict(vec![
            ("kind".into(), PyValue::Str("intermediate".into())),
            ("spoken".into(), PyValue::Str("first".into())),
        ]);
        assert_eq!(
            value.dumps(),
            r#"{"kind": "intermediate", "spoken": "first"}"#
        );
    }

    #[test]
    fn dumps_leaves_non_ascii_alone() {
        let value = PyValue::Str("Three failures — all in auth.py.".into());
        assert_eq!(value.dumps(), "\"Three failures — all in auth.py.\"");
    }

    #[test]
    fn dumps_escapes_the_c0_set_and_nothing_else() {
        let value = PyValue::Str("a \"q\" \\ b\nc\td\u{1}/e".into());
        assert_eq!(value.dumps(), r#""a \"q\" \\ b\nc\td\u0001/e""#);
    }

    #[test]
    fn ints_stay_integral_and_floats_keep_a_point() {
        assert_eq!(PyValue::Int(7).dumps(), "7");
        assert_eq!(PyValue::Float(1.15).dumps(), "1.15");
        assert_eq!(PyValue::Float(1000000.0).dumps(), "1000000.0");
    }

    #[test]
    fn loads_round_trips_and_keeps_key_order() {
        let text = r#"{"z": 1, "a": [1, 2.5, "x", true, null], "m": {"k": "v"}}"#;
        let value = PyValue::loads(text).expect("parses");
        assert_eq!(value.dumps(), text);
        match &value {
            PyValue::Dict(pairs) => {
                assert_eq!(pairs[0].0, "z");
                assert_eq!(pairs[1].0, "a");
                assert_eq!(pairs[2].0, "m");
            }
            _ => panic!("expected a dict"),
        }
    }

    #[test]
    fn loads_decodes_escapes_including_surrogate_pairs() {
        let value = PyValue::loads(r#""a—b😀""#).expect("parses");
        assert_eq!(value.as_str(), Some("a—b😀"));
    }

    #[test]
    fn record_from_fixture_reads_pairs_and_rejects_anything_else() {
        let value: serde_json::Value =
            serde_json::from_str(r#"[["b", 1], ["a", [[1, 2], [3, 4]]]]"#).expect("json");
        let record = record_from_fixture(&value).expect("pairs");
        assert_eq!(record[0].0, "b");
        assert_eq!(record[1].0, "a");
        // The inner value is a LIST of two-element lists, not a dict - which is
        // precisely the case the old shape heuristic got wrong.
        assert_eq!(
            PyValue::Dict(record).dumps(),
            r#"{"b": 1, "a": [[1, 2], [3, 4]]}"#
        );
        assert!(record_from_fixture(&serde_json::json!({"a": 1})).is_none());
        assert!(record_from_fixture(&serde_json::json!([["a", 1, 2]])).is_none());
    }

    #[test]
    fn loads_rejects_trailing_garbage() {
        assert!(PyValue::loads(r#"{"a": 1} nope"#).is_none());
        assert!(PyValue::loads("not json").is_none());
    }
}
