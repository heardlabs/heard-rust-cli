//! An order-preserving JSON value.
//!
//! A user's `settings.json` is theirs: rewriting it with every key sorted
//! alphabetically would be a noisy diff even though it means the same thing.
//! `serde_json`'s `preserve_order` feature would fix that, but cargo unifies
//! features across the workspace and every other crate's `Map` would change
//! order with it. This small value type keeps objects as a `Vec` of pairs,
//! so a round trip through the installer only changes what it means to.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};

/// An object: its members in file order.
pub type Obj = Vec<(String, Json)>;

/// A JSON value whose objects remember their key order.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(serde_json::Number),
    Str(String),
    Arr(Vec<Json>),
    Obj(Obj),
}

impl Json {
    /// Parse text. Trailing whitespace is allowed; anything else is an error.
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Two-space indented, with a trailing newline — the format
    /// `json.dumps(indent=2)` and Claude Code itself write.
    pub fn to_pretty(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into());
        s.push('\n');
        s
    }

    /// Into a `serde_json::Value` — for semantic (order-blind) comparison.
    pub fn to_value(&self) -> serde_json::Value {
        use serde_json::Value;
        match self {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Num(n) => Value::Number(n.clone()),
            Json::Str(s) => Value::String(s.clone()),
            Json::Arr(a) => Value::Array(a.iter().map(Json::to_value).collect()),
            Json::Obj(o) => {
                Value::Object(o.iter().map(|(k, v)| (k.clone(), v.to_value())).collect())
            }
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_obj(&self) -> Option<&Obj> {
        match self {
            Json::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_obj_mut(&mut self) -> Option<&mut Obj> {
        match self {
            Json::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_arr_mut(&mut self) -> Option<&mut Vec<Json>> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    /// Member `key` of an object (the first, if the file repeats a key).
    pub fn get(&self, key: &str) -> Option<&Json> {
        self.as_obj()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        self.as_obj_mut()?
            .iter_mut()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
}

impl Json {
    /// Exactly what Python's `json.dumps(value, indent=indent)` prints with
    /// its defaults: `ensure_ascii` (every non-ASCII character as `\\uXXXX`,
    /// surrogate pairs above the BMP), `", "` / `": "` separators when
    /// compact, `","` + newline + `indent` spaces per level when indented,
    /// keys in their order, floats as Python's `repr`. No trailing newline.
    /// For files an older Python writer produced and a reader may compare
    /// byte for byte.
    pub fn dumps_py(&self, indent: Option<usize>) -> String {
        let mut out = String::new();
        py_write(self, indent, 0, &mut out);
        out
    }
}

fn py_write(v: &Json, indent: Option<usize>, level: usize, out: &mut String) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Num(n) => out.push_str(&py_number(n)),
        Json::Str(s) => py_string(s, out),
        Json::Arr(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                py_sep(i, indent, level + 1, out);
                py_write(x, indent, level + 1, out);
            }
            py_close(indent, level, out);
            out.push(']');
        }
        Json::Obj(o) => {
            if o.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                py_sep(i, indent, level + 1, out);
                py_string(k, out);
                out.push_str(": ");
                py_write(x, indent, level + 1, out);
            }
            py_close(indent, level, out);
            out.push('}');
        }
    }
}

fn py_sep(i: usize, indent: Option<usize>, level: usize, out: &mut String) {
    match indent {
        None => {
            if i > 0 {
                out.push_str(", ");
            }
        }
        Some(n) => {
            if i > 0 {
                out.push(',');
            }
            out.push('\n');
            out.push_str(&" ".repeat(n * level));
        }
    }
}

fn py_close(indent: Option<usize>, level: usize, out: &mut String) {
    if let Some(n) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(n * level));
    }
}

fn py_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python's `float.__repr__` (and `int` as-is).
fn py_number(n: &serde_json::Number) -> String {
    if n.is_i64() || n.is_u64() {
        return n.to_string();
    }
    let Some(f) = n.as_f64() else {
        return n.to_string();
    };
    py_float_repr(f)
}

/// `repr(f)` for a finite float: the shortest round-trip digits, in fixed
/// notation when the decimal exponent is in [-4, 16), else `d.ddde±XX`.
pub fn py_float_repr(f: f64) -> String {
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    // Rust's `{:e}` is the shortest round-trip form: `-1.2345e-7`.
    let sci = format!("{f:e}");
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (neg, mant) = match mant.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mant),
    };
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let sign = if neg { "-" } else { "" };
    if (-4..16).contains(&exp) {
        let point = exp + 1; // digits before the decimal point
        let body = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if (point as usize) >= digits.len() {
            format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
        } else {
            let (a, b) = digits.split_at(point as usize);
            format!("{a}.{b}")
        };
        format!("{sign}{body}")
    } else {
        let m = if digits.len() > 1 {
            format!("{}.{}", &digits[..1], &digits[1..])
        } else {
            digits.clone()
        };
        let es = if exp < 0 { '-' } else { '+' };
        format!("{sign}{m}e{es}{:02}", exp.abs())
    }
}

/// Look up `key`, appending `default` at the end when it is missing.
pub fn entry<'a>(obj: &'a mut Obj, key: &str, default: impl FnOnce() -> Json) -> &'a mut Json {
    let idx = match obj.iter().position(|(k, _)| k == key) {
        Some(i) => i,
        None => {
            obj.push((key.to_owned(), default()));
            obj.len() - 1
        }
    };
    &mut obj[idx].1
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Json::Null => s.serialize_unit(),
            Json::Bool(b) => s.serialize_bool(*b),
            Json::Num(n) => n.serialize(s),
            Json::Str(v) => s.serialize_str(v),
            Json::Arr(a) => {
                let mut seq = s.serialize_seq(Some(a.len()))?;
                for v in a {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            Json::Obj(o) => {
                let mut map = s.serialize_map(Some(o.len()))?;
                for (k, v) in o {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_unit<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }
    fn visit_none<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }
    fn visit_bool<E>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Num(v.into()))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Json, E> {
        Ok(Json::Num(v.into()))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Json, E> {
        serde_json::Number::from_f64(v)
            .map(Json::Num)
            .ok_or_else(|| E::custom("non-finite number"))
    }
    fn visit_str<E>(self, v: &str) -> Result<Json, E> {
        Ok(Json::Str(v.to_owned()))
    }
    fn visit_string<E>(self, v: String) -> Result<Json, E> {
        Ok(Json::Str(v))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element()? {
            out.push(v);
        }
        Ok(Json::Arr(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut out = Vec::new();
        while let Some((k, v)) = map.next_entry::<String, Json>()? {
            out.push((k, v));
        }
        Ok(Json::Obj(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_key_order_and_numbers() {
        let text = "{\n  \"z\": 1,\n  \"a\": [\n    true,\n    null,\n    2.5,\n    -3\n  ],\n  \"m\": {}\n}\n";
        let v = Json::parse(text).unwrap();
        assert_eq!(v.to_pretty(), text);
    }

    #[test]
    fn dumps_py_matches_python_defaults() {
        let v = Json::parse(
            r#"{"b":[1,2.5,"é😀"],"a":{},"c":[],"d":null,"e":1e16,"f":0.0001,"g":1e-5,"h":100.0}"#,
        )
        .unwrap();
        assert_eq!(
            v.dumps_py(None),
            r#"{"b": [1, 2.5, "\u00e9\ud83d\ude00"], "a": {}, "c": [], "d": null, "e": 1e+16, "f": 0.0001, "g": 1e-05, "h": 100.0}"#
        );
        assert_eq!(
            Json::parse(r#"{"a":[1,{"x":true}]}"#)
                .unwrap()
                .dumps_py(Some(2)),
            "{\n  \"a\": [\n    1,\n    {\n      \"x\": true\n    }\n  ]\n}"
        );
        assert_eq!(py_float_repr(1790158530.123456), "1790158530.123456");
        assert_eq!(py_float_repr(-2.5e-7), "-2.5e-07");
        assert_eq!(
            py_float_repr(123456789012345680.0),
            "1.2345678901234568e+17"
        );
    }

    #[test]
    fn empty_containers_stay_compact() {
        let v = Json::parse(r#"{"a":[],"b":{}}"#).unwrap();
        assert_eq!(v.to_pretty(), "{\n  \"a\": [],\n  \"b\": {}\n}\n");
    }
}
