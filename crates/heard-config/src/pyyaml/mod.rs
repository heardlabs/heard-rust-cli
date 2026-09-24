//! PyYAML, ported: `yaml.safe_load` and `yaml.safe_dump(..., sort_keys=True,
//! allow_unicode=True)` exactly as `engine/heard/config.py` calls them.
//!
//! `config.yaml` is shared between the Python engine and the Rust port, so a
//! YAML 1.2 library is the wrong tool twice over: it READS `1e+16` as a float
//! (PyYAML, which is YAML 1.1, reads it as a string), `yes` as a string
//! (PyYAML: `True`), `12:30` as a string (PyYAML: `750`); and it WRITES text
//! Python never would — no folding at 80 columns, `|-` blocks for multi-line
//! strings, different quoting. Both directions are therefore ported from
//! PyYAML 6's pure-Python code (the engine uses `yaml.safe_load` /
//! `yaml.safe_dump`, i.e. the pure-Python `SafeLoader`/`SafeDumper`, not
//! libyaml): reader + scanner + parser + composer + `SafeConstructor` on the
//! way in, `SafeRepresenter` + serializer + emitter on the way out.
//! `fixtures/config/yaml_{dump,load}.json` (made by the fixture generator)
//! pins both directions byte for byte.
//!
//! # Values serde_json cannot hold
//!
//! The crate's currency is `serde_json::Value`. PyYAML can produce a few
//! Python values JSON cannot: non-finite floats (`.inf`, `-.inf`, `.nan`),
//! ints wider than 64 bits, dates and datetimes, `bytes` (`!!binary`) and sets
//! (`!!set`). Each becomes a one-key object whose key is the YAML tag and
//! whose value is the canonical text PyYAML would write back —
//! `{"!!float": ".inf"}`, `{"!!int": "18446744073709551616"}`,
//! `{"!!timestamp": "2026-09-23"}`, `{"!!binary": "<base64>"}`,
//! `{"!!set": [..]}`. The dumper writes them back as PyYAML does, and
//! [`non_finite_float`] / [`float_value`] let a reader that must behave like
//! Python on a float (`int(.inf)` raises `OverflowError`) see the real `f64`.

mod emitter;
mod loader;
mod parser;
mod resolver;
mod scanner;

use scanner::Mark;
use serde_json::{Map, Number, Value};
use std::fmt;

/// Sentinel key for a float JSON cannot hold (`.inf`, `-.inf`, `.nan`).
pub const FLOAT_TAG: &str = "!!float";
/// Sentinel key for an int outside `i64`/`u64`.
pub const INT_TAG: &str = "!!int";
/// Sentinel key for a `datetime.date` / `datetime.datetime`.
pub const TIMESTAMP_TAG: &str = "!!timestamp";
/// Sentinel key for `bytes` (`!!binary`).
pub const BINARY_TAG: &str = "!!binary";
/// Sentinel key for a `set` (`!!set`).
pub const SET_TAG: &str = "!!set";

/// Why a document did not load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// A `yaml.YAMLError` — what `config._read` catches, logs and treats as
    /// a corrupt file.
    Yaml,
    /// Some OTHER Python exception escaping `safe_load` (`ValueError` from
    /// `!!timestamp 2026-02-30`, `KeyError` from `!!bool maybe`, …). Python's
    /// `_read` does not catch these, so `load()` raises.
    Python,
}

/// A failed load.
#[derive(Debug, Clone)]
pub struct YamlError {
    /// Which kind of Python exception this would have been.
    pub kind: ErrorKind,
    /// A PyYAML-shaped message (for logs; not byte-matched).
    pub message: String,
}

impl fmt::Display for YamlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for YamlError {}

impl YamlError {
    fn new(context: Option<&str>, problem: String, mark: Mark) -> Self {
        Self::marked(context, None, problem, mark)
    }

    fn marked(
        context: Option<&str>,
        context_mark: Option<Mark>,
        problem: String,
        mark: Mark,
    ) -> Self {
        let mut lines = Vec::new();
        if let Some(c) = context {
            lines.push(c.to_string());
        }
        if let Some(cm) = context_mark {
            if cm != mark {
                lines.push(where_(cm));
            }
        }
        lines.push(problem);
        lines.push(where_(mark));
        YamlError {
            kind: ErrorKind::Yaml,
            message: lines.join("\n"),
        }
    }

    fn reader(message: String) -> Self {
        YamlError {
            kind: ErrorKind::Yaml,
            message,
        }
    }

    fn value(message: String) -> Self {
        YamlError {
            kind: ErrorKind::Python,
            message,
        }
    }
}

fn where_(m: Mark) -> String {
    format!(
        "  in \"<file>\", line {}, column {}",
        m.line + 1,
        m.column + 1
    )
}

/// Python's text-mode newline translation (`open(..., encoding="utf-8")`):
/// `\r\n` and a lone `\r` both become `\n` before PyYAML ever sees the text.
pub fn universal_newlines(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\r') {
        return std::borrow::Cow::Borrowed(text);
    }
    std::borrow::Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// `yaml.safe_load(text)`, as a JSON value (see the module docs for the
/// sentinels). `text` is taken as Python's text-mode read would hand it over,
/// so `\r\n` is translated first.
///
/// # Errors
///
/// [`YamlError`] with [`ErrorKind::Yaml`] where PyYAML raises a `YAMLError`,
/// [`ErrorKind::Python`] where some other exception escapes.
pub fn safe_load(text: &str) -> Result<Value, YamlError> {
    Ok(load_py(text)?.to_json())
}

/// `yaml.safe_load(text)` as the Python value model (keeps truthiness and
/// key types, which `config.load`'s `or {}` needs).
pub(crate) fn load_py(text: &str) -> Result<Py, YamlError> {
    loader::safe_load(&universal_newlines(text))
}

/// `yaml.safe_dump(value, sort_keys=True, allow_unicode=True)` — the text
/// `config.save()` writes.
pub fn safe_dump(value: &Value) -> String {
    emitter::dump(value)
}

/// The real float behind a value, if it is one JSON cannot hold
/// (`{"!!float": ".inf"}` and friends).
pub fn non_finite_float(v: &Value) -> Option<f64> {
    let Value::Object(m) = v else { return None };
    if m.len() != 1 {
        return None;
    }
    match m.get(FLOAT_TAG)?.as_str()? {
        ".inf" => Some(f64::INFINITY),
        "-.inf" => Some(f64::NEG_INFINITY),
        ".nan" => Some(f64::NAN),
        _ => None,
    }
}

/// A float as a config value: a JSON number when finite, the `!!float`
/// sentinel otherwise.
pub fn float_value(f: f64) -> Value {
    match Number::from_f64(f) {
        Some(n) => Value::Number(n),
        None => {
            let text = if f.is_nan() {
                ".nan"
            } else if f > 0.0 {
                ".inf"
            } else {
                "-.inf"
            };
            let mut m = Map::new();
            m.insert(FLOAT_TAG.into(), Value::String(text.into()));
            Value::Object(m)
        }
    }
}

// ── the Python value model ──────────────────────────────────────────────────

/// What `SafeConstructor` can build.
#[derive(Clone, Debug)]
pub(crate) enum Py {
    None,
    Bool(bool),
    Int(BigInt),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Date(PyDate),
    DateTime(PyDateTime),
    List(Vec<Py>),
    /// Only from `!!omap` / `!!pairs` entries.
    Tuple(Vec<Py>),
    Dict(Vec<(Py, Py)>),
    Set(Vec<Py>),
}

impl Py {
    /// `bool(x)`.
    pub(crate) fn truthy(&self) -> bool {
        match self {
            Py::None => false,
            Py::Bool(b) => *b,
            Py::Int(n) => !n.is_zero(),
            Py::Float(f) => *f != 0.0,
            Py::Str(s) => !s.is_empty(),
            Py::Bytes(b) => !b.is_empty(),
            Py::Date(_) | Py::DateTime(_) => true,
            Py::List(v) | Py::Tuple(v) | Py::Set(v) => !v.is_empty(),
            Py::Dict(d) => !d.is_empty(),
        }
    }

    /// The YAML kind name `config.load` reports for a non-mapping document.
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Py::None => "null",
            Py::Bool(_) => "bool",
            Py::Int(_) | Py::Float(_) => "number",
            Py::Str(_) => "string",
            Py::Bytes(_) => "binary",
            Py::Date(_) | Py::DateTime(_) => "timestamp",
            Py::List(_) | Py::Tuple(_) => "sequence",
            Py::Dict(_) => "mapping",
            Py::Set(_) => "set",
        }
    }

    fn hashable(&self) -> bool {
        match self {
            Py::List(_) | Py::Dict(_) | Py::Set(_) => false,
            Py::Tuple(v) => v.iter().all(Py::hashable),
            _ => true,
        }
    }

    /// Python `==` as a dict key sees it. Numbers compare across bool, int
    /// and float; NaN keys made by `.nan` are the same object in PyYAML
    /// (`SafeConstructor.nan_value`), so they collapse too.
    fn py_eq(&self, other: &Py) -> bool {
        use Py as P;
        if let (Some(a), Some(b)) = (self.as_number(), other.as_number()) {
            return a.eq(&b);
        }
        match (self, other) {
            (P::None, P::None) => true,
            (P::Str(a), P::Str(b)) => a == b,
            (P::Bytes(a), P::Bytes(b)) => a == b,
            (P::Date(a), P::Date(b)) => a == b,
            (P::DateTime(a), P::DateTime(b)) => a.py_eq(b),
            (P::Tuple(a), P::Tuple(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.py_eq(y))
            }
            _ => false,
        }
    }

    fn as_number(&self) -> Option<Num> {
        match self {
            Py::Bool(b) => Some(Num::Int(BigInt::from_u64(u64::from(*b)))),
            Py::Int(n) => Some(Num::Int(n.clone())),
            Py::Float(f) => Some(Num::Float(*f)),
            _ => None,
        }
    }

    /// Into the crate's currency (see the module docs).
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Py::None => Value::Null,
            Py::Bool(b) => Value::Bool(*b),
            Py::Int(n) => n.to_json(),
            Py::Float(f) => float_value(*f),
            Py::Str(s) => Value::String(s.clone()),
            Py::Bytes(b) => sentinel(BINARY_TAG, Value::String(base64_encode(b))),
            Py::Date(d) => sentinel(TIMESTAMP_TAG, Value::String(d.isoformat())),
            Py::DateTime(d) => sentinel(TIMESTAMP_TAG, Value::String(d.isoformat())),
            Py::List(v) | Py::Tuple(v) => Value::Array(v.iter().map(Py::to_json).collect()),
            Py::Dict(pairs) => {
                let mut m = Map::new();
                for (k, v) in pairs {
                    m.insert(k.key_string(), v.to_json());
                }
                Value::Object(m)
            }
            Py::Set(items) => {
                let mut v: Vec<Value> = items.iter().map(Py::to_json).collect();
                v.sort_by_key(|x| x.to_string());
                sentinel(SET_TAG, Value::Array(v))
            }
        }
    }

    /// A dict key as a string, the way `json.dumps` spells the ones it can
    /// (`True` → `"true"`, `1.5` → `"1.5"`, `None` → `"null"`); a date key
    /// uses its isoformat.
    pub(crate) fn key_string(&self) -> String {
        match self {
            Py::None => "null".into(),
            Py::Bool(b) => if *b { "true" } else { "false" }.into(),
            Py::Int(n) => n.to_string(),
            Py::Float(f) => {
                if f.is_nan() {
                    "NaN".into()
                } else if f.is_infinite() {
                    if *f > 0.0 { "Infinity" } else { "-Infinity" }.into()
                } else {
                    py_float_repr(*f)
                }
            }
            Py::Str(s) => s.clone(),
            Py::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            Py::Date(d) => d.isoformat(),
            Py::DateTime(d) => d.isoformat(),
            other => other.to_json().to_string(),
        }
    }
}

fn sentinel(tag: &str, v: Value) -> Value {
    let mut m = Map::new();
    m.insert(tag.into(), v);
    Value::Object(m)
}

enum Num {
    Int(BigInt),
    Float(f64),
}

impl Num {
    fn eq(&self, other: &Num) -> bool {
        match (self, other) {
            (Num::Int(a), Num::Int(b)) => a == b,
            (Num::Float(a), Num::Float(b)) => a == b || (a.is_nan() && b.is_nan()),
            (Num::Int(i), Num::Float(f)) | (Num::Float(f), Num::Int(i)) => {
                if *f == 0.0 {
                    i.is_zero()
                } else {
                    f.is_finite() && f.fract() == 0.0 && i.to_string() == format!("{f:.0}")
                }
            }
        }
    }
}

// ── arbitrary-precision ints (Python's int is unbounded) ────────────────────

/// A sign-magnitude integer, base 10⁹ limbs, little-endian. Only what the
/// int constructor needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BigInt {
    pub negative: bool,
    limbs: Vec<u32>,
}

const LIMB: u64 = 1_000_000_000;

impl BigInt {
    pub(crate) fn zero() -> Self {
        BigInt {
            negative: false,
            limbs: Vec::new(),
        }
    }

    pub(crate) fn from_u64(mut v: u64) -> Self {
        let mut limbs = Vec::new();
        while v > 0 {
            limbs.push((v % LIMB) as u32);
            v /= LIMB;
        }
        BigInt {
            negative: false,
            limbs,
        }
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    /// `self = self * m + a` for small `m`, `a` (magnitude only).
    pub(crate) fn mul_add(&mut self, m: u32, a: u32) {
        let mut carry = u64::from(a);
        for limb in &mut self.limbs {
            let v = u64::from(*limb) * u64::from(m) + carry;
            *limb = (v % LIMB) as u32;
            carry = v / LIMB;
        }
        while carry > 0 {
            self.limbs.push((carry % LIMB) as u32);
            carry /= LIMB;
        }
    }

    pub(crate) fn mul(&self, other: &BigInt) -> BigInt {
        let mut out = vec![0u64; self.limbs.len() + other.limbs.len() + 1];
        for (i, a) in self.limbs.iter().enumerate() {
            let mut carry = 0u64;
            for (j, b) in other.limbs.iter().enumerate() {
                let v = out[i + j] + u64::from(*a) * u64::from(*b) + carry;
                out[i + j] = v % LIMB;
                carry = v / LIMB;
            }
            let mut k = i + other.limbs.len();
            while carry > 0 {
                let v = out[k] + carry;
                out[k] = v % LIMB;
                carry = v / LIMB;
                k += 1;
            }
        }
        let mut limbs: Vec<u32> = out.into_iter().map(|v| v as u32).collect();
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
        BigInt {
            negative: false,
            limbs,
        }
    }

    /// Magnitude addition (both operands are non-negative here).
    pub(crate) fn add(&self, other: &BigInt) -> BigInt {
        let n = self.limbs.len().max(other.limbs.len());
        let mut limbs = Vec::with_capacity(n + 1);
        let mut carry = 0u64;
        for i in 0..n {
            let v = u64::from(*self.limbs.get(i).unwrap_or(&0))
                + u64::from(*other.limbs.get(i).unwrap_or(&0))
                + carry;
            limbs.push((v % LIMB) as u32);
            carry = v / LIMB;
        }
        if carry > 0 {
            limbs.push(carry as u32);
        }
        BigInt {
            negative: false,
            limbs,
        }
    }

    fn to_json(&self) -> Value {
        let s = self.to_string();
        if let Ok(i) = s.parse::<i64>() {
            return Value::from(i);
        }
        if let Ok(u) = s.parse::<u64>() {
            return Value::from(u);
        }
        sentinel(INT_TAG, Value::String(s))
    }
}

impl fmt::Display for BigInt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.limbs.is_empty() {
            return f.write_str("0");
        }
        let mut s = String::new();
        if self.negative {
            s.push('-');
        }
        let mut it = self.limbs.iter().rev();
        if let Some(top) = it.next() {
            s.push_str(&top.to_string());
        }
        for limb in it {
            s.push_str(&format!("{limb:09}"));
        }
        f.write_str(&s)
    }
}

// ── dates ───────────────────────────────────────────────────────────────────

/// `datetime.date`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PyDate {
    year: u32,
    month: u32,
    day: u32,
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) {
                29
            } else {
                28
            }
        }
    }
}

impl PyDate {
    pub(crate) fn new(year: u32, month: u32, day: u32) -> Result<Self, YamlError> {
        if !(1..=9999).contains(&year) {
            return Err(YamlError::value(format!("year {year} is out of range")));
        }
        if !(1..=12).contains(&month) {
            return Err(YamlError::value("month must be in 1..12".into()));
        }
        if day < 1 || day > days_in_month(year, month) {
            return Err(YamlError::value("day is out of range for month".into()));
        }
        Ok(PyDate { year, month, day })
    }

    fn isoformat(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// Days since 0001-01-01, for comparing aware datetimes by instant.
    fn ordinal(&self) -> i64 {
        let y = i64::from(self.year) - 1;
        let mut days = y * 365 + y / 4 - y / 100 + y / 400;
        for m in 1..self.month {
            days += i64::from(days_in_month(self.year, m));
        }
        days + i64::from(self.day)
    }
}

/// `datetime.datetime`, naive or with a fixed offset (in minutes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PyDateTime {
    date: PyDate,
    hour: u32,
    minute: u32,
    second: u32,
    microsecond: u32,
    offset_minutes: Option<i64>,
}

impl PyDateTime {
    pub(crate) fn new(
        date: PyDate,
        hour: u32,
        minute: u32,
        second: u32,
        microsecond: u32,
        offset_minutes: Option<i64>,
    ) -> Result<Self, YamlError> {
        if hour > 23 {
            return Err(YamlError::value("hour must be in 0..23".into()));
        }
        if minute > 59 {
            return Err(YamlError::value("minute must be in 0..59".into()));
        }
        if second > 59 {
            return Err(YamlError::value("second must be in 0..59".into()));
        }
        Ok(PyDateTime {
            date,
            hour,
            minute,
            second,
            microsecond,
            offset_minutes,
        })
    }

    /// `isoformat(' ')` — what `SafeRepresenter.represent_datetime` writes.
    fn isoformat(&self) -> String {
        let mut s = format!(
            "{} {:02}:{:02}:{:02}",
            self.date.isoformat(),
            self.hour,
            self.minute,
            self.second
        );
        if self.microsecond != 0 {
            s.push_str(&format!(".{:06}", self.microsecond));
        }
        if let Some(off) = self.offset_minutes {
            let sign = if off < 0 { '-' } else { '+' };
            let a = off.abs();
            s.push_str(&format!("{sign}{:02}:{:02}", a / 60, a % 60));
        }
        s
    }

    fn micros_utc(&self) -> i128 {
        let secs = self.date.ordinal() * 86_400
            + i64::from(self.hour) * 3600
            + i64::from(self.minute) * 60
            + i64::from(self.second)
            - self.offset_minutes.unwrap_or(0) * 60;
        i128::from(secs) * 1_000_000 + i128::from(self.microsecond)
    }

    /// Python: naive == naive by fields, aware == aware by instant, and a
    /// naive never equals an aware one.
    fn py_eq(&self, other: &PyDateTime) -> bool {
        match (self.offset_minutes, other.offset_minutes) {
            (None, None) => self == other,
            (Some(_), Some(_)) => self.micros_utc() == other.micros_utc(),
            _ => false,
        }
    }
}

// ── base64 (`!!binary`) ─────────────────────────────────────────────────────

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `base64.decodebytes` — CPython's non-strict `binascii.a2b_base64`, step for
/// step: characters outside the alphabet are skipped, and a complete pad
/// sequence ends the input (anything after it is ignored).
pub(crate) fn base64_decode_lenient(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut leftchar: u8 = 0;
    let mut quad = 0;
    let mut pads = 0;
    for c in text.bytes() {
        if c == b'=' {
            if quad >= 2 {
                pads += 1;
                if quad + pads >= 4 {
                    return Ok(out);
                }
            }
            continue;
        }
        let Some(v) = B64.iter().position(|b| *b == c) else {
            continue;
        };
        let v = v as u8;
        pads = 0;
        match quad {
            0 => {
                quad = 1;
                leftchar = v;
            }
            1 => {
                quad = 2;
                out.push((leftchar << 2) | (v >> 4));
                leftchar = v & 0x0f;
            }
            2 => {
                quad = 3;
                out.push((leftchar << 4) | (v >> 2));
                leftchar = v & 0x03;
            }
            _ => {
                quad = 0;
                out.push((leftchar << 6) | v);
                leftchar = 0;
            }
        }
    }
    if quad != 0 {
        if quad == 1 {
            return Err(
                "Invalid base64-encoded string: number of data characters cannot be 1 more than a multiple of 4"
                    .into(),
            );
        }
        return Err("Incorrect padding".into());
    }
    Ok(out)
}

// ── floats ──────────────────────────────────────────────────────────────────

/// Python's `repr(float)` for a finite float: the shortest round-tripping
/// digits, positional when the exponent is in `-4..16`, else `d.ddde±XX`.
pub(crate) fn py_float_repr(f: f64) -> String {
    let sci = format!("{f:e}");
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (neg, mant) = match mant.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mant),
    };
    let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        if exp < 0 {
            out.push_str("0.");
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        } else {
            let point = exp as usize + 1;
            if digits.len() <= point {
                out.push_str(&digits);
                for _ in digits.len()..point {
                    out.push('0');
                }
                out.push_str(".0");
            } else {
                out.push_str(&digits[..point]);
                out.push('.');
                out.push_str(&digits[point..]);
            }
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", exp.abs()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_repr_matches_python() {
        for (f, want) in [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (0.5, "0.5"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (1.5e-7, "1.5e-07"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (123.456, "123.456"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
            (0.1, "0.1"),
        ] {
            assert_eq!(py_float_repr(f), want, "{f}");
        }
    }

    #[test]
    fn bigint_prints() {
        let mut n = BigInt::zero();
        for d in "123456789012345678901234567890".chars() {
            n.mul_add(10, d.to_digit(10).unwrap());
        }
        assert_eq!(n.to_string(), "123456789012345678901234567890");
    }
}
