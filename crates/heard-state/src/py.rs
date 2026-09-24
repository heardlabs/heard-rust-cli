//! The handful of CPython behaviours this crate has to reproduce exactly.
//!
//! Ported code builds spoken text and on-disk JSON out of Python built-ins,
//! and a port that reached for the "obvious" Rust equivalent would drift in
//! ways a reader would never spot:
//!
//! * `str.strip()` / `\s` treat `\x1c`–`\x1f` as whitespace; Rust's
//!   `char::is_whitespace` does not.
//! * `len(s)` and `s[:n]` count code points, not bytes.
//! * `json.dumps` puts a space after `:` and `,`, keeps insertion order, and
//!   spells floats with `repr` (`1e-05`, `1e+16`); `indent=2` has its own
//!   layout.
//! * `bool(x)` / `int(x)` on a config value are Python truthiness and
//!   Python's `int()` constructor, not serde's.
//!
//! [`PyValue`] (from [`crate::pyjson`]) is the order-preserving value these
//! all operate on.

use std::fmt::Write as _;

use crate::pyjson::PyValue;
use serde_json::Value;

/// `str.isspace()` for one character.
#[must_use]
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str.strip()`.
#[must_use]
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `str.rstrip()`.
#[must_use]
pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

/// `str.split()` with no arguments.
pub fn split_ws(s: &str) -> impl Iterator<Item = &str> {
    s.split(is_space).filter(|p| !p.is_empty())
}

/// `len(s)`.
#[must_use]
pub fn len(s: &str) -> usize {
    s.chars().count()
}

/// `s[:n]` for a non-negative `n`.
#[must_use]
pub fn head(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The regex class Python's `re` means by `\s` in a `str` pattern.
pub const WS: &str = r"[\s\x1C-\x1F]";

/// `bool(value)` for a config value.
#[must_use]
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `int(value)` for a config value; `None` where Python raises.
#[must_use]
pub fn int_of(value: &Value) -> Option<i64> {
    match value {
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite())
                // int(3.9) == 3, int(-3.9) == -3: truncation toward zero.
                .map(|f| f.trunc() as i64)
        }),
        Value::String(s) => {
            let t = strip(s).replace('_', "");
            t.parse::<i64>().ok()
        }
        _ => None,
    }
}

/// `int(value)` for a JSON value read back off disk; `None` where Python
/// raises.
#[must_use]
pub fn int_of_py(value: &PyValue) -> Option<i64> {
    match value {
        PyValue::Bool(b) => Some(i64::from(*b)),
        PyValue::Int(n) => Some(*n),
        PyValue::Float(f) if f.is_finite() => Some(f.trunc() as i64),
        PyValue::Str(s) => strip(s).replace('_', "").parse::<i64>().ok(),
        _ => None,
    }
}

/// `float(value)`; `None` where Python raises.
#[must_use]
pub fn float_of_py(value: &PyValue) -> Option<f64> {
    match value {
        PyValue::Bool(b) => Some(f64::from(u8::from(*b))),
        #[allow(clippy::cast_precision_loss)]
        PyValue::Int(n) => Some(*n as f64),
        PyValue::Float(f) => Some(*f),
        PyValue::Str(s) => {
            let t = strip(s).to_ascii_lowercase();
            match t.as_str() {
                "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
                "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
                "nan" | "+nan" | "-nan" => Some(f64::NAN),
                _ => t.parse::<f64>().ok(),
            }
        }
        _ => None,
    }
}

/// `bool(value)` for a JSON value.
#[must_use]
pub fn truthy_py(value: Option<&PyValue>) -> bool {
    match value {
        None | Some(PyValue::Null) => false,
        Some(PyValue::Bool(b)) => *b,
        Some(PyValue::Int(n)) => *n != 0,
        Some(PyValue::Float(f)) => *f != 0.0,
        Some(PyValue::Str(s)) => !s.is_empty(),
        Some(PyValue::List(l)) => !l.is_empty(),
        Some(PyValue::Dict(d)) => !d.is_empty(),
    }
}

/// `x or default` — the value if truthy, else `None`.
#[must_use]
pub fn or(value: Option<&PyValue>) -> Option<&PyValue> {
    value.filter(|v| truthy_py(Some(v)))
}

/// `str(value)` for a scalar the way an f-string renders it.
#[must_use]
pub fn str_of(value: &PyValue) -> String {
    match value {
        PyValue::Null => "None".into(),
        PyValue::Bool(true) => "True".into(),
        PyValue::Bool(false) => "False".into(),
        PyValue::Int(n) => n.to_string(),
        PyValue::Float(f) => float_repr(*f),
        PyValue::Str(s) => s.clone(),
        // Containers are rendered as JSON — `str()` of a dict is its repr,
        // which no caller here ever reaches with well-formed data.
        other => dumps(other),
    }
}

/// `float.__repr__`: shortest round-trip digits, fixed notation for
/// `1e-4 <= |x| < 1e16`, otherwise `d.ddde±XX`.
#[must_use]
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    // `{:e}` is shortest-round-trip too: "1.2345e3", "-5e-7".
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        let n = digits.len() as i32;
        if exp >= 0 {
            let int_len = exp + 1;
            if n <= int_len {
                out.push_str(&digits);
                for _ in 0..(int_len - n) {
                    out.push('0');
                }
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len as usize]);
                out.push('.');
                out.push_str(&digits[int_len as usize..]);
            }
        } else {
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    out
}

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

fn write_scalar(value: &PyValue, out: &mut String) -> bool {
    match value {
        PyValue::Null => out.push_str("null"),
        PyValue::Bool(true) => out.push_str("true"),
        PyValue::Bool(false) => out.push_str("false"),
        PyValue::Int(n) => {
            let _ = write!(out, "{n}");
        }
        PyValue::Float(f) => out.push_str(&float_repr(*f)),
        PyValue::Str(s) => write_string(s, out),
        _ => return false,
    }
    true
}

fn write_compact(value: &PyValue, out: &mut String) {
    if write_scalar(value, out) {
        return;
    }
    match value {
        PyValue::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_compact(item, out);
            }
            out.push(']');
        }
        PyValue::Dict(pairs) => {
            out.push('{');
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_string(k, out);
                out.push_str(": ");
                write_compact(v, out);
            }
            out.push('}');
        }
        _ => {}
    }
}

fn write_indented(value: &PyValue, indent: usize, level: usize, out: &mut String) {
    if write_scalar(value, out) {
        return;
    }
    let pad = |out: &mut String, lvl: usize| {
        out.push('\n');
        for _ in 0..indent * lvl {
            out.push(' ');
        }
    };
    match value {
        PyValue::List(items) if items.is_empty() => out.push_str("[]"),
        PyValue::Dict(pairs) if pairs.is_empty() => out.push_str("{}"),
        PyValue::List(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                pad(out, level + 1);
                write_indented(item, indent, level + 1, out);
            }
            pad(out, level);
            out.push(']');
        }
        PyValue::Dict(pairs) => {
            out.push('{');
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                pad(out, level + 1);
                write_string(k, out);
                out.push_str(": ");
                write_indented(v, indent, level + 1, out);
            }
            pad(out, level);
            out.push('}');
        }
        _ => {}
    }
}

/// `json.dumps(value, ensure_ascii=False)`.
#[must_use]
pub fn dumps(value: &PyValue) -> String {
    let mut out = String::new();
    write_compact(value, &mut out);
    out
}

/// `json.dumps(value, ensure_ascii=False, indent=indent)`.
#[must_use]
pub fn dumps_indent(value: &PyValue, indent: usize) -> String {
    let mut out = String::new();
    write_indented(value, indent, 0, &mut out);
    out
}

/// `json.loads`, order-preserving. `None` where Python raises.
#[must_use]
pub fn loads(text: &str) -> Option<PyValue> {
    PyValue::loads(text)
}

/// `bytes.decode("utf-8", errors="ignore")`.
#[must_use]
pub fn decode_ignore(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
    }
    out
}

/// A dict, built in insertion order.
#[must_use]
pub fn dict(pairs: Vec<(&str, PyValue)>) -> PyValue {
    PyValue::Dict(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

/// `list[-n:]` for any integer `n`, Python slice semantics.
#[must_use]
pub fn tail_slice<T>(items: &[T], n: i64) -> &[T] {
    let len = items.len() as i64;
    // `-n` as a start index.
    let start = -n;
    let start = if start < 0 {
        (len + start).max(0)
    } else {
        start.min(len)
    };
    &items[start as usize..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_repr_matches_cpython() {
        for (f, want) in [
            (1.0, "1.0"),
            (1000.5, "1000.5"),
            (1e-05, "1e-05"),
            (0.0001, "0.0001"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (123456789012345.6, "123456789012345.6"),
            (-2.5, "-2.5"),
            (0.1, "0.1"),
            (1_000_000.0, "1000000.0"),
            (1e22, "1e+22"),
            (1.7e-7, "1.7e-07"),
        ] {
            assert_eq!(float_repr(f), want, "{f}");
        }
    }

    #[test]
    fn indent_layout_matches_json_dumps() {
        let v = dict(vec![
            ("a", PyValue::List(vec![])),
            ("b", dict(vec![])),
            (
                "c",
                PyValue::List(vec![PyValue::Int(1), dict(vec![("x", PyValue::Null)])]),
            ),
        ]);
        assert_eq!(
            dumps_indent(&v, 2),
            "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": [\n    1,\n    {\n      \"x\": null\n    }\n  ]\n}"
        );
        assert_eq!(
            dumps(&v),
            "{\"a\": [], \"b\": {}, \"c\": [1, {\"x\": null}]}"
        );
    }

    #[test]
    fn slices_and_strips_are_pythons() {
        assert_eq!(tail_slice(&[1, 2, 3, 4], 2), &[3, 4]);
        assert_eq!(tail_slice(&[1, 2, 3, 4], 0), &[1, 2, 3, 4]);
        assert_eq!(tail_slice(&[1, 2, 3, 4], 9), &[1, 2, 3, 4]);
        assert_eq!(tail_slice(&[1, 2, 3, 4], -1), &[2, 3, 4]);
        assert_eq!(strip("\x1c hi \u{a0}"), "hi");
        assert_eq!(head("héllo", 2), "hé");
        assert_eq!(decode_ignore(b"a\xffb"), "ab");
    }

    #[test]
    fn config_coercions() {
        assert_eq!(int_of(&serde_json::json!(8)), Some(8));
        assert_eq!(int_of(&serde_json::json!(3.9)), Some(3));
        assert_eq!(int_of(&serde_json::json!(" 12 ")), Some(12));
        assert_eq!(int_of(&serde_json::json!("x")), None);
        assert_eq!(int_of(&serde_json::json!(null)), None);
        assert!(truthy(Some(&serde_json::json!("yes"))));
        assert!(!truthy(Some(&serde_json::json!(0))));
    }
}
