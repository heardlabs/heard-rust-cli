//! `yaml/composer.py` + `yaml/constructor.py` (`SafeConstructor`).
//!
//! Nodes live in an arena so aliases share them exactly as Python's node
//! objects are shared, and `flatten_mapping`'s in-place edits (it deletes
//! merge keys and retags `=` keys) are seen by every later use of the node,
//! as they are in Python.

use super::parser::{Ev, Parser};
use super::resolver::{self, parse_timestamp};
use super::scanner::{Mark, Scanner};
use super::{BigInt, Py, PyDate, PyDateTime, YamlError};
use std::collections::HashMap;

#[derive(Clone, Debug)]
enum Kind {
    Scalar(String),
    Seq(Vec<usize>),
    Map(Vec<(usize, usize)>),
}

#[derive(Clone, Debug)]
struct Node {
    tag: String,
    kind: Kind,
    start: Mark,
}

impl Node {
    fn id(&self) -> &'static str {
        match self.kind {
            Kind::Scalar(_) => "scalar",
            Kind::Seq(_) => "sequence",
            Kind::Map(_) => "mapping",
        }
    }
}

type R<T> = Result<T, YamlError>;

/// Python's recursion limit, roughly: PyYAML composes and constructs
/// recursively, so absurdly deep nesting ends in `RecursionError` there. The
/// exact depth is interpreter-dependent; this keeps Rust off the end of its
/// stack and reports the same kind of failure.
const MAX_DEPTH: usize = 400;

struct Composer {
    parser: Parser,
    nodes: Vec<Node>,
    anchors: HashMap<String, usize>,
}

impl Composer {
    fn next(&mut self) -> R<Ev> {
        Ok(self
            .parser
            .get_event()?
            .expect("the parser always ends with a stream-end event")
            .kind)
    }

    fn peek(&mut self) -> R<(Ev, Mark)> {
        let e = self
            .parser
            .peek_event()?
            .expect("the parser always ends with a stream-end event");
        Ok((e.kind.clone(), e.start))
    }

    /// `Composer.get_single_node`.
    fn single_node(&mut self) -> R<Option<usize>> {
        self.next()?; // StreamStart
        let mut document = None;
        if !matches!(self.peek()?.0, Ev::StreamEnd) {
            document = Some(self.compose_document()?);
        }
        let (ev, mark) = self.peek()?;
        if !matches!(ev, Ev::StreamEnd) {
            return Err(YamlError::marked(
                Some("expected a single document in the stream"),
                document.map(|d| self.nodes[d].start),
                "but found another document".into(),
                mark,
            ));
        }
        self.next()?;
        Ok(document)
    }

    fn compose_document(&mut self) -> R<usize> {
        self.next()?; // DocumentStart
        let node = self.compose_node(0)?;
        self.next()?; // DocumentEnd
        self.anchors.clear();
        Ok(node)
    }

    fn compose_node(&mut self, depth: usize) -> R<usize> {
        if depth > MAX_DEPTH {
            return Err(YamlError::value("maximum recursion depth exceeded".into()));
        }
        let (ev, mark) = self.peek()?;
        if let Ev::Alias(name) = &ev {
            self.next()?;
            return match self.anchors.get(name) {
                Some(&n) => Ok(n),
                None => Err(YamlError::marked(
                    None,
                    None,
                    format!("found undefined alias '{name}'"),
                    mark,
                )),
            };
        }
        let anchor = match &ev {
            Ev::Scalar { anchor, .. }
            | Ev::SequenceStart { anchor, .. }
            | Ev::MappingStart { anchor, .. } => anchor.clone(),
            _ => None,
        };
        if let Some(a) = &anchor {
            if let Some(&first) = self.anchors.get(a) {
                return Err(YamlError::marked(
                    Some(&format!("found duplicate anchor '{a}'; first occurrence")),
                    Some(self.nodes[first].start),
                    "second occurrence".into(),
                    mark,
                ));
            }
        }
        match ev {
            Ev::Scalar {
                tag,
                implicit,
                value,
                ..
            } => {
                self.next()?;
                let tag = match tag {
                    Some(t) if t != "!" => t,
                    _ => resolver::resolve_scalar(&value, implicit.0).to_string(),
                };
                let id = self.push(Node {
                    tag,
                    kind: Kind::Scalar(value),
                    start: mark,
                });
                if let Some(a) = anchor {
                    self.anchors.insert(a, id);
                }
                Ok(id)
            }
            Ev::SequenceStart { tag, .. } => {
                self.next()?;
                let tag = match tag {
                    Some(t) if t != "!" => t,
                    _ => resolver::SEQ.to_string(),
                };
                let id = self.push(Node {
                    tag,
                    kind: Kind::Seq(Vec::new()),
                    start: mark,
                });
                if let Some(a) = anchor {
                    self.anchors.insert(a, id);
                }
                while !matches!(self.peek()?.0, Ev::SequenceEnd) {
                    let child = self.compose_node(depth + 1)?;
                    if let Kind::Seq(v) = &mut self.nodes[id].kind {
                        v.push(child);
                    }
                }
                self.next()?;
                Ok(id)
            }
            Ev::MappingStart { tag, .. } => {
                self.next()?;
                let tag = match tag {
                    Some(t) if t != "!" => t,
                    _ => resolver::MAP.to_string(),
                };
                let id = self.push(Node {
                    tag,
                    kind: Kind::Map(Vec::new()),
                    start: mark,
                });
                if let Some(a) = anchor {
                    self.anchors.insert(a, id);
                }
                while !matches!(self.peek()?.0, Ev::MappingEnd) {
                    let k = self.compose_node(depth + 1)?;
                    let v = self.compose_node(depth + 1)?;
                    if let Kind::Map(pairs) = &mut self.nodes[id].kind {
                        pairs.push((k, v));
                    }
                }
                self.next()?;
                Ok(id)
            }
            other => unreachable!("the parser never yields {other:?} where a node belongs"),
        }
    }

    fn push(&mut self, node: Node) -> usize {
        self.nodes.push(node);
        self.nodes.len() - 1
    }
}

/// `yaml.safe_load(text)` into the Python value model.
pub(crate) fn safe_load(text: &str) -> R<Py> {
    let scanner = Scanner::new(text)?;
    let mut composer = Composer {
        parser: Parser::new(scanner),
        nodes: Vec::new(),
        anchors: HashMap::new(),
    };
    let root = composer.single_node()?;
    if composer.parser.saw_surrogate() {
        return Err(YamlError::value(
            "a surrogate escape: Python loads a lone surrogate, which a Rust String cannot hold"
                .into(),
        ));
    }
    let Some(root) = root else {
        return Ok(Py::None);
    };
    let mut c = Constructor {
        nodes: composer.nodes,
        constructed: HashMap::new(),
        in_progress: Vec::new(),
    };
    c.construct_object(root, 0)
}

struct Constructor {
    nodes: Vec<Node>,
    constructed: HashMap<usize, Py>,
    in_progress: Vec<usize>,
}

fn cerr(context: Option<&str>, problem: String, mark: Mark) -> YamlError {
    YamlError::marked(context, None, problem, mark)
}

impl Constructor {
    /// `BaseConstructor.construct_object`. Python hands back the SAME object
    /// for an aliased node; the value model is plain data, so a clone is the
    /// same value. A node that (through an alias) contains itself is a
    /// recursive structure Python can build and plain data cannot — refused.
    fn construct_object(&mut self, id: usize, depth: usize) -> R<Py> {
        if let Some(v) = self.constructed.get(&id) {
            return Ok(v.clone());
        }
        if depth > MAX_DEPTH {
            return Err(YamlError::value("maximum recursion depth exceeded".into()));
        }
        if self.in_progress.contains(&id) {
            return Err(YamlError::value(
                "a recursive YAML structure (an alias inside its own anchor) cannot be represented"
                    .into(),
            ));
        }
        self.in_progress.push(id);
        let tag = self.nodes[id].tag.clone();
        let result = self.construct_tagged(&tag, id, depth);
        self.in_progress.pop();
        let v = result?;
        self.constructed.insert(id, v.clone());
        Ok(v)
    }

    fn construct_tagged(&mut self, tag: &str, id: usize, depth: usize) -> R<Py> {
        let mark = self.nodes[id].start;
        match tag {
            resolver::NULL => {
                self.construct_scalar(id)?;
                Ok(Py::None)
            }
            resolver::BOOL => {
                let v = self.construct_scalar(id)?;
                match v.to_lowercase().as_str() {
                    "yes" | "true" | "on" => Ok(Py::Bool(true)),
                    "no" | "false" | "off" => Ok(Py::Bool(false)),
                    _ => Err(YamlError::value(format!(
                        "KeyError: {:?}",
                        v.to_lowercase()
                    ))),
                }
            }
            resolver::INT => {
                let v = self.construct_scalar(id)?;
                construct_int(&v).map(Py::Int)
            }
            resolver::FLOAT => {
                let v = self.construct_scalar(id)?;
                construct_float(&v).map(Py::Float)
            }
            resolver::BINARY => {
                let v = self.construct_scalar(id)?;
                if !v.is_ascii() {
                    return Err(cerr(
                        None,
                        "failed to convert base64 data into ascii".into(),
                        mark,
                    ));
                }
                super::base64_decode_lenient(&v)
                    .map(Py::Bytes)
                    .map_err(|e| cerr(None, format!("failed to decode base64 data: {e}"), mark))
            }
            resolver::TIMESTAMP => {
                let v = self.construct_scalar(id)?;
                construct_timestamp(&v)
            }
            resolver::OMAP | resolver::PAIRS => {
                let what = if tag == resolver::OMAP {
                    "while constructing an ordered map"
                } else {
                    "while constructing pairs"
                };
                let Kind::Seq(items) = self.nodes[id].kind.clone() else {
                    return Err(cerr(
                        Some(what),
                        format!("expected a sequence, but found {}", self.nodes[id].id()),
                        mark,
                    ));
                };
                let mut out = Vec::new();
                for sub in items {
                    let Kind::Map(pairs) = self.nodes[sub].kind.clone() else {
                        return Err(cerr(
                            Some(what),
                            format!(
                                "expected a mapping of length 1, but found {}",
                                self.nodes[sub].id()
                            ),
                            self.nodes[sub].start,
                        ));
                    };
                    if pairs.len() != 1 {
                        return Err(cerr(
                            Some(what),
                            format!(
                                "expected a single mapping item, but found {} items",
                                pairs.len()
                            ),
                            self.nodes[sub].start,
                        ));
                    }
                    let (k, v) = pairs[0];
                    let key = self.construct_object(k, depth + 1)?;
                    let value = self.construct_object(v, depth + 1)?;
                    out.push(Py::Tuple(vec![key, value]));
                }
                Ok(Py::List(out))
            }
            resolver::SET => {
                let pairs = self.construct_mapping(id, depth)?;
                let mut out: Vec<Py> = Vec::new();
                for (k, _) in pairs {
                    if !out.iter().any(|e| e.py_eq(&k)) {
                        out.push(k);
                    }
                }
                Ok(Py::Set(out))
            }
            resolver::STR => Ok(Py::Str(self.construct_scalar(id)?)),
            resolver::SEQ => {
                let Kind::Seq(items) = self.nodes[id].kind.clone() else {
                    return Err(cerr(
                        None,
                        format!(
                            "expected a sequence node, but found {}",
                            self.nodes[id].id()
                        ),
                        mark,
                    ));
                };
                let mut out = Vec::with_capacity(items.len());
                for child in items {
                    out.push(self.construct_object(child, depth + 1)?);
                }
                Ok(Py::List(out))
            }
            resolver::MAP => Ok(Py::Dict(self.construct_mapping(id, depth)?)),
            other => Err(cerr(
                None,
                format!("could not determine a constructor for the tag '{other}'"),
                mark,
            )),
        }
    }

    /// `SafeConstructor.construct_scalar`.
    fn construct_scalar(&self, id: usize) -> R<String> {
        let node = &self.nodes[id];
        if let Kind::Map(pairs) = &node.kind {
            for (k, v) in pairs {
                if self.nodes[*k].tag == resolver::VALUE {
                    return self.construct_scalar(*v);
                }
            }
        }
        match &node.kind {
            Kind::Scalar(s) => Ok(s.clone()),
            _ => Err(cerr(
                None,
                format!("expected a scalar node, but found {}", node.id()),
                node.start,
            )),
        }
    }

    /// `SafeConstructor.flatten_mapping` — mutates the node, as Python does.
    fn flatten_mapping(&mut self, id: usize) -> R<()> {
        let Kind::Map(mut pairs) = self.nodes[id].kind.clone() else {
            return Ok(());
        };
        let mut merge: Vec<(usize, usize)> = Vec::new();
        let mut index = 0;
        while index < pairs.len() {
            let (k, v) = pairs[index];
            if self.nodes[k].tag == resolver::MERGE {
                pairs.remove(index);
                match self.nodes[v].kind.clone() {
                    Kind::Map(_) => {
                        self.flatten_mapping(v)?;
                        if let Kind::Map(sub) = &self.nodes[v].kind {
                            merge.extend(sub.iter().copied());
                        }
                    }
                    Kind::Seq(subnodes) => {
                        let mut submerge: Vec<Vec<(usize, usize)>> = Vec::new();
                        for sub in subnodes {
                            if !matches!(self.nodes[sub].kind, Kind::Map(_)) {
                                return Err(YamlError::marked(
                                    Some("while constructing a mapping"),
                                    Some(self.nodes[id].start),
                                    format!(
                                        "expected a mapping for merging, but found {}",
                                        self.nodes[sub].id()
                                    ),
                                    self.nodes[sub].start,
                                ));
                            }
                            self.flatten_mapping(sub)?;
                            if let Kind::Map(p) = &self.nodes[sub].kind {
                                submerge.push(p.clone());
                            }
                        }
                        submerge.reverse();
                        for value in submerge {
                            merge.extend(value);
                        }
                    }
                    Kind::Scalar(_) => {
                        return Err(YamlError::marked(
                            Some("while constructing a mapping"),
                            Some(self.nodes[id].start),
                            format!(
                                "expected a mapping or list of mappings for merging, but found {}",
                                self.nodes[v].id()
                            ),
                            self.nodes[v].start,
                        ));
                    }
                }
            } else if self.nodes[k].tag == resolver::VALUE {
                self.nodes[k].tag = resolver::STR.to_string();
                index += 1;
            } else {
                index += 1;
            }
        }
        if !merge.is_empty() {
            merge.extend(pairs);
            pairs = merge;
        }
        self.nodes[id].kind = Kind::Map(pairs);
        Ok(())
    }

    /// `SafeConstructor.construct_mapping`, keeping Python dict semantics:
    /// a repeated key keeps its first position and takes the last value, and
    /// keys compare the way Python's do (`1 == 1.0 == True`).
    fn construct_mapping(&mut self, id: usize, depth: usize) -> R<Vec<(Py, Py)>> {
        if matches!(self.nodes[id].kind, Kind::Map(_)) {
            self.flatten_mapping(id)?;
        }
        let Kind::Map(pairs) = self.nodes[id].kind.clone() else {
            return Err(cerr(
                None,
                format!("expected a mapping node, but found {}", self.nodes[id].id()),
                self.nodes[id].start,
            ));
        };
        let mut out: Vec<(Py, Py)> = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            let key = self.construct_object(k, depth + 1)?;
            if !key.hashable() {
                return Err(YamlError::marked(
                    Some("while constructing a mapping"),
                    Some(self.nodes[id].start),
                    "found unhashable key".into(),
                    self.nodes[k].start,
                ));
            }
            let value = self.construct_object(v, depth + 1)?;
            if let Some(slot) = out.iter_mut().find(|(ek, _)| ek.py_eq(&key)) {
                slot.1 = value;
            } else {
                out.push((key, value));
            }
        }
        Ok(out)
    }
}

// ── scalar constructors ─────────────────────────────────────────────────────

/// Python's `int(text, base)` for the ASCII texts PyYAML hands it: surrounding
/// whitespace, an optional sign, the base's own `0x`/`0o`/`0b` prefix, and
/// single underscores between digits.
fn py_int(text: &str, base: u32) -> R<BigInt> {
    let bad = || {
        YamlError::value(format!(
            "invalid literal for int() with base {base}: {text:?}"
        ))
    };
    let t = text.trim_matches(|c: char| c.is_whitespace());
    let (neg, mut body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let prefix = match base {
        16 => Some(["0x", "0X"]),
        8 => Some(["0o", "0O"]),
        2 => Some(["0b", "0B"]),
        _ => None,
    };
    if let Some(ps) = prefix {
        for p in ps {
            if let Some(rest) = body.strip_prefix(p) {
                body = rest.strip_prefix('_').unwrap_or(rest);
                break;
            }
        }
    }
    if body.is_empty() || body.starts_with('_') || body.ends_with('_') || body.contains("__") {
        return Err(bad());
    }
    let mut n = BigInt::zero();
    for c in body.chars() {
        if c == '_' {
            continue;
        }
        let d = c.to_digit(base).ok_or_else(bad)?;
        n.mul_add(base, d);
    }
    // Python rejects leading zeros in a base-10 literal ("007") — but only
    // for int(x) with an explicit literal, not for int("007"): int() of a
    // STRING accepts leading zeros in base 10. Nothing to do.
    n.negative = neg && !n.is_zero();
    Ok(n)
}

/// `SafeConstructor.construct_yaml_int`.
pub(crate) fn construct_int(text: &str) -> R<BigInt> {
    let mut value: String = text.replace('_', "");
    let Some(first) = value.chars().next() else {
        return Err(YamlError::value(
            "IndexError: string index out of range".into(),
        ));
    };
    let neg = first == '-';
    if first == '-' || first == '+' {
        value = value[1..].to_string();
    }
    let signed = |mut n: BigInt| {
        if neg {
            n.negative = !n.negative && !n.is_zero();
        }
        n
    };
    if value == "0" {
        return Ok(BigInt::zero());
    }
    if let Some(rest) = value.strip_prefix("0b") {
        return py_int(rest, 2).map(signed);
    }
    if let Some(rest) = value.strip_prefix("0x") {
        return py_int(rest, 16).map(signed);
    }
    if value.starts_with('0') {
        return py_int(&value, 8).map(signed);
    }
    if value.contains(':') {
        let mut digits = Vec::new();
        for part in value.split(':') {
            digits.push(py_int(part, 10)?);
        }
        digits.reverse();
        let mut total = BigInt::zero();
        let mut base = BigInt::from_u64(1);
        for d in digits {
            total = total.add(&d.mul(&base));
            base = base.mul(&BigInt::from_u64(60));
        }
        return Ok(signed(total));
    }
    // `value[0]` raised IndexError above for ""; a bare sign leaves "" here.
    py_int(&value, 10).map(signed)
}

/// Python's `float(text)`.
pub(crate) fn py_float(text: &str) -> R<f64> {
    let t = text.trim_matches(|c: char| c.is_whitespace());
    let bad = || YamlError::value(format!("could not convert string to float: {text:?}"));
    let lower = t.to_ascii_lowercase();
    let body = lower.trim_start_matches(['+', '-']);
    if lower.len() - body.len() > 1 {
        return Err(bad());
    }
    // Rust accepts the same decimal grammar once underscores are gone, plus
    // "inf"/"infinity"/"nan" — which Python accepts too.
    if body.is_empty()
        || !body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'+' || b == b'-')
    {
        return Err(bad());
    }
    if body.contains('_') {
        return Err(bad());
    }
    lower.parse::<f64>().map_err(|_| bad())
}

/// `SafeConstructor.construct_yaml_float`.
pub(crate) fn construct_float(text: &str) -> R<f64> {
    let mut value = text.replace('_', "").to_lowercase();
    let Some(first) = value.chars().next() else {
        return Err(YamlError::value(
            "IndexError: string index out of range".into(),
        ));
    };
    let sign = if first == '-' { -1.0 } else { 1.0 };
    if first == '-' || first == '+' {
        value = value[1..].to_string();
    }
    if value == ".inf" {
        return Ok(sign * f64::INFINITY);
    }
    if value == ".nan" {
        return Ok(f64::NAN);
    }
    if value.contains(':') {
        let mut digits = Vec::new();
        for part in value.split(':') {
            digits.push(py_float(part)?);
        }
        digits.reverse();
        let mut base: f64 = 1.0;
        let mut total = 0.0;
        for d in digits {
            total += d * base;
            base *= 60.0;
        }
        return Ok(sign * total);
    }
    Ok(sign * py_float(&value)?)
}

/// `SafeConstructor.construct_yaml_timestamp`.
fn construct_timestamp(text: &str) -> R<Py> {
    let Some(p) = parse_timestamp(text, false) else {
        return Err(YamlError::value(
            "AttributeError: 'NoneType' object has no attribute 'groupdict'".into(),
        ));
    };
    let num = |s: &str| s.parse::<u32>().unwrap_or(0);
    let date = PyDate::new(num(p.year), num(p.month), num(p.day))?;
    let Some(t) = p.time else {
        return Ok(Py::Date(date));
    };
    let hour = num(t.hour);
    let minute = num(t.minute);
    let second = num(t.second);
    let mut fraction = 0u32;
    if let Some(f) = t.fraction.filter(|f| !f.is_empty()) {
        let mut f: String = f.chars().take(6).collect();
        while f.len() < 6 {
            f.push('0');
        }
        fraction = num(&f);
    }
    let offset_minutes = match t.tz {
        None => None,
        Some(None) => Some(0),
        Some(Some((sign, h, m))) => {
            let mins = i64::from(num(h)) * 60 + i64::from(m.map(num).unwrap_or(0));
            Some(if sign == b'-' { -mins } else { mins })
        }
    };
    if let Some(off) = offset_minutes {
        if off.abs() >= 24 * 60 {
            return Err(YamlError::value(
                "offset must be a timedelta strictly between -timedelta(hours=24) and timedelta(hours=24)"
                    .into(),
            ));
        }
    }
    Ok(Py::DateTime(PyDateTime::new(
        date,
        hour,
        minute,
        second,
        fraction,
        offset_minutes,
    )?))
}
