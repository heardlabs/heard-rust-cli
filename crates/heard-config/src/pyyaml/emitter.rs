//! `yaml/representer.py` (`SafeRepresenter`) + `yaml/serializer.py` +
//! `yaml/emitter.py`, for `safe_dump(data, sort_keys=True,
//! allow_unicode=True)` with every other option at its default:
//! `default_flow_style=False`, `indent=2`, `width=80`, `line_break='\n'`, no
//! explicit document markers, no canonical form.
//!
//! PyYAML's emitter is an event-driven state machine; the whole tree is in
//! hand here, so each `expect_*` state becomes a method that recurses into the
//! children, keeping the emitter's mutable state (`indent`, `column`,
//! `whitespace`, `indention`, …) and every write routine exactly as they are.
//! The folding at 80 columns, the choice between plain, single-quoted and
//! double-quoted, the `\` line continuations in double quotes: all of it is in
//! the write routines below, character for character.
//!
//! Anchors are never emitted: PyYAML only writes `&id001` when the SAME Python
//! list/dict object appears twice, and JSON values have no identity.

use super::resolver::{self, BINARY, BOOL, FLOAT, INT, MAP, NULL, SEQ, SET, STR, TIMESTAMP};
use super::{py_float_repr, BINARY_TAG, FLOAT_TAG, INT_TAG, SET_TAG, TIMESTAMP_TAG};
use serde_json::Value;

// ── representer ─────────────────────────────────────────────────────────────

enum Node {
    Scalar {
        tag: &'static str,
        value: Vec<char>,
        style: Option<char>,
    },
    Seq {
        tag: &'static str,
        items: Vec<Node>,
    },
    Map {
        tag: &'static str,
        pairs: Vec<(Node, Node)>,
    },
}

fn scalar(tag: &'static str, value: &str) -> Node {
    Node::Scalar {
        tag,
        value: value.chars().collect(),
        style: None,
    }
}

/// A one-key `{"!!tag": …}` object: which sentinel, and its payload.
fn as_sentinel(m: &serde_json::Map<String, Value>) -> Option<(&str, &Value)> {
    if m.len() != 1 {
        return None;
    }
    let (k, v) = m.iter().next()?;
    let ok = match k.as_str() {
        FLOAT_TAG => matches!(v.as_str(), Some(".inf" | "-.inf" | ".nan")),
        INT_TAG | TIMESTAMP_TAG | BINARY_TAG => v.is_string(),
        SET_TAG => v.is_array(),
        _ => false,
    };
    ok.then_some((k.as_str(), v))
}

/// `SafeRepresenter.represent_data`.
fn represent(v: &Value) -> Node {
    match v {
        Value::Null => scalar(NULL, "null"),
        Value::Bool(b) => scalar(BOOL, if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                scalar(INT, &i.to_string())
            } else if let Some(u) = n.as_u64() {
                scalar(INT, &u.to_string())
            } else {
                scalar(FLOAT, &represent_float(n.as_f64().unwrap_or(0.0)))
            }
        }
        Value::String(s) => scalar(STR, s),
        Value::Array(items) => Node::Seq {
            tag: SEQ,
            items: items.iter().map(represent).collect(),
        },
        Value::Object(m) => {
            if let Some((tag, payload)) = as_sentinel(m) {
                return represent_sentinel(tag, payload);
            }
            // `sorted(mapping.items())`: str keys, compared by code point —
            // which is exactly UTF-8 byte order.
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            Node::Map {
                tag: MAP,
                pairs: keys
                    .into_iter()
                    .map(|k| (scalar(STR, k), represent(&m[k])))
                    .collect(),
            }
        }
    }
}

fn represent_sentinel(tag: &str, payload: &Value) -> Node {
    let text = payload.as_str().unwrap_or("");
    match tag {
        FLOAT_TAG => scalar(FLOAT, text),
        INT_TAG => scalar(INT, text),
        TIMESTAMP_TAG => scalar(TIMESTAMP, text),
        BINARY_TAG => {
            let bytes = super::base64_decode_lenient(text).unwrap_or_default();
            Node::Scalar {
                tag: BINARY,
                value: encodebytes(&bytes).chars().collect(),
                style: Some('|'),
            }
        }
        _ => {
            // `represent_set`: a mapping of each element to None, sorted when
            // the elements are mutually comparable.
            let mut items: Vec<&Value> = payload
                .as_array()
                .map(|a| a.iter().collect())
                .unwrap_or_default();
            if items.iter().all(|v| v.is_string()) {
                items.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
            } else if items.iter().all(|v| v.is_number()) {
                items.sort_by(|a, b| {
                    a.as_f64()
                        .partial_cmp(&b.as_f64())
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            Node::Map {
                tag: SET,
                pairs: items
                    .into_iter()
                    .map(|k| (represent(k), scalar(NULL, "null")))
                    .collect(),
            }
        }
    }
}

/// `base64.encodebytes`: 76-character lines, each ending in `\n`.
fn encodebytes(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(57) {
        out.push_str(&super::base64_encode(chunk));
        out.push('\n');
    }
    out
}

/// `SafeRepresenter.represent_float` for a finite float.
fn represent_float(f: f64) -> String {
    let value = py_float_repr(f).to_lowercase();
    if !value.contains('.') && value.contains('e') {
        value.replacen('e', ".0e", 1)
    } else {
        value
    }
}

// ── emitter ─────────────────────────────────────────────────────────────────

const BEST_INDENT: isize = 2;
const BEST_WIDTH: usize = 80;

fn is_break(ch: char) -> bool {
    matches!(ch, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

fn is_blankz(ch: char) -> bool {
    matches!(
        ch,
        '\0' | ' ' | '\t' | '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

#[derive(Clone, Copy)]
struct Analysis {
    empty: bool,
    multiline: bool,
    allow_flow_plain: bool,
    allow_block_plain: bool,
    allow_single_quoted: bool,
    allow_block: bool,
}

/// `Emitter.analyze_scalar` (with `allow_unicode=True`).
fn analyze_scalar(s: &[char]) -> Analysis {
    if s.is_empty() {
        return Analysis {
            empty: true,
            multiline: false,
            allow_flow_plain: false,
            allow_block_plain: true,
            allow_single_quoted: true,
            allow_block: false,
        };
    }
    let mut block_indicators = false;
    let mut flow_indicators = false;
    let mut line_breaks = false;
    let mut special_characters = false;
    let mut leading_space = false;
    let mut leading_break = false;
    let mut trailing_space = false;
    let mut trailing_break = false;
    let mut break_space = false;
    let mut space_break = false;
    let starts = |p: &str| s.len() >= 3 && s[..3].iter().copied().eq(p.chars());
    if starts("---") || starts("...") {
        block_indicators = true;
        flow_indicators = true;
    }
    let mut preceded_by_whitespace = true;
    let mut followed_by_whitespace = s.len() == 1 || is_blankz(s[1]);
    let mut previous_space = false;
    let mut previous_break = false;
    let n = s.len();
    for (index, &ch) in s.iter().enumerate() {
        if index == 0 {
            if "#,[]{}&*!|>'\"%@`".contains(ch) {
                flow_indicators = true;
                block_indicators = true;
            }
            if ch == '?' || ch == ':' {
                flow_indicators = true;
                if followed_by_whitespace {
                    block_indicators = true;
                }
            }
            if ch == '-' && followed_by_whitespace {
                flow_indicators = true;
                block_indicators = true;
            }
        } else {
            if ",?[]{}".contains(ch) {
                flow_indicators = true;
            }
            if ch == ':' {
                flow_indicators = true;
                if followed_by_whitespace {
                    block_indicators = true;
                }
            }
            if ch == '#' && preceded_by_whitespace {
                flow_indicators = true;
                block_indicators = true;
            }
        }
        if is_break(ch) {
            line_breaks = true;
        }
        if !(ch == '\n' || ('\x20'..='\x7E').contains(&ch)) {
            let unicode = (ch == '\u{85}'
                || ('\u{A0}'..='\u{D7FF}').contains(&ch)
                || ('\u{E000}'..='\u{FFFD}').contains(&ch)
                || ('\u{10000}'..'\u{10FFFF}').contains(&ch))
                && ch != '\u{FEFF}';
            if !unicode {
                special_characters = true;
            }
        }
        if ch == ' ' {
            if index == 0 {
                leading_space = true;
            }
            if index == n - 1 {
                trailing_space = true;
            }
            if previous_break {
                break_space = true;
            }
            previous_space = true;
            previous_break = false;
        } else if is_break(ch) {
            if index == 0 {
                leading_break = true;
            }
            if index == n - 1 {
                trailing_break = true;
            }
            if previous_space {
                space_break = true;
            }
            previous_space = false;
            previous_break = true;
        } else {
            previous_space = false;
            previous_break = false;
        }
        let next = index + 1;
        preceded_by_whitespace = is_blankz(ch);
        followed_by_whitespace = next + 1 >= n || is_blankz(s[next + 1]);
    }
    let mut allow_flow_plain = true;
    let mut allow_block_plain = true;
    let mut allow_single_quoted = true;
    let mut allow_block = true;
    if leading_space || leading_break || trailing_space || trailing_break {
        allow_flow_plain = false;
        allow_block_plain = false;
    }
    if trailing_space {
        allow_block = false;
    }
    if break_space {
        allow_flow_plain = false;
        allow_block_plain = false;
        allow_single_quoted = false;
    }
    if space_break || special_characters {
        allow_flow_plain = false;
        allow_block_plain = false;
        allow_single_quoted = false;
        allow_block = false;
    }
    if line_breaks {
        allow_flow_plain = false;
        allow_block_plain = false;
    }
    if flow_indicators {
        allow_flow_plain = false;
    }
    if block_indicators {
        allow_block_plain = false;
    }
    Analysis {
        empty: false,
        multiline: line_breaks,
        allow_flow_plain,
        allow_block_plain,
        allow_single_quoted,
        allow_block,
    }
}

/// `Emitter.prepare_tag` for the `tag:yaml.org,2002:` tags this dumper emits.
fn prepare_tag(tag: &str) -> String {
    match tag.strip_prefix("tag:yaml.org,2002:") {
        Some(suffix) if !suffix.is_empty() => format!("!!{suffix}"),
        _ => format!("!<{tag}>"),
    }
}

struct Emitter {
    out: String,
    indents: Vec<Option<isize>>,
    indent: Option<isize>,
    flow_level: usize,
    root_context: bool,
    mapping_context: bool,
    simple_key_context: bool,
    column: usize,
    whitespace: bool,
    indention: bool,
    open_ended: bool,
}

/// The whole of `yaml.safe_dump(data, sort_keys=True, allow_unicode=True)`.
pub(crate) fn dump(value: &Value) -> String {
    let node = represent(value);
    let mut e = Emitter {
        out: String::new(),
        indents: Vec::new(),
        indent: None,
        flow_level: 0,
        root_context: false,
        mapping_context: false,
        simple_key_context: false,
        column: 0,
        whitespace: true,
        indention: true,
        open_ended: false,
    };
    // DocumentStart: implicit (first document, nothing explicit, and the
    // document is never the empty-scalar special case because every node
    // carries a tag).
    e.expect_node(&node, true, false, false, false);
    // DocumentEnd (implicit).
    e.write_indent();
    // StreamEnd.
    if e.open_ended {
        e.write_indicator("...", true, false, false);
        e.write_indent();
    }
    e.out
}

impl Emitter {
    fn increase_indent(&mut self, flow: bool, indentless: bool) {
        self.indents.push(self.indent);
        match self.indent {
            None => self.indent = Some(if flow { BEST_INDENT } else { 0 }),
            Some(i) if !indentless => self.indent = Some(i + BEST_INDENT),
            Some(_) => {}
        }
    }

    fn pop_indent(&mut self) {
        self.indent = self.indents.pop().flatten();
    }

    fn expect_node(
        &mut self,
        node: &Node,
        root: bool,
        _sequence: bool,
        mapping: bool,
        simple_key: bool,
    ) {
        self.root_context = root;
        self.mapping_context = mapping;
        self.simple_key_context = simple_key;
        match node {
            Node::Scalar { tag, value, style } => {
                let analysis = analyze_scalar(value);
                let implicit = (
                    resolver::resolve_scalar(&value.iter().collect::<String>(), true) == *tag,
                    STR == *tag,
                );
                let chosen = self.choose_scalar_style(&analysis, implicit, *style);
                // process_tag
                let skip = (chosen.is_none() && implicit.0) || (chosen.is_some() && implicit.1);
                if !skip {
                    self.write_indicator(&prepare_tag(tag), true, false, false);
                }
                // expect_scalar
                self.increase_indent(true, false);
                let split = !self.simple_key_context;
                match chosen {
                    Some('"') => self.write_double_quoted(value, split),
                    Some('\'') => self.write_single_quoted(value, split),
                    Some('|') => self.write_literal(value),
                    _ => self.write_plain(value, split),
                }
                self.pop_indent();
            }
            Node::Seq { tag, items } => {
                if *tag != SEQ {
                    self.write_indicator(&prepare_tag(tag), true, false, false);
                }
                if self.flow_level > 0 || items.is_empty() {
                    self.expect_flow_sequence(items);
                } else {
                    self.expect_block_sequence(items);
                }
            }
            Node::Map { tag, pairs } => {
                if *tag != MAP {
                    self.write_indicator(&prepare_tag(tag), true, false, false);
                }
                if self.flow_level > 0 || pairs.is_empty() {
                    self.expect_flow_mapping(pairs);
                } else {
                    self.expect_block_mapping(pairs);
                }
            }
        }
    }

    /// `Emitter.choose_scalar_style`; `None` is the plain style (`''`).
    fn choose_scalar_style(
        &self,
        a: &Analysis,
        implicit: (bool, bool),
        style: Option<char>,
    ) -> Option<char> {
        if style == Some('"') {
            return Some('"');
        }
        if style.is_none()
            && implicit.0
            && !(self.simple_key_context && (a.empty || a.multiline))
            && ((self.flow_level > 0 && a.allow_flow_plain)
                || (self.flow_level == 0 && a.allow_block_plain))
        {
            return None;
        }
        if let Some(s @ ('|' | '>')) = style {
            if self.flow_level == 0 && !self.simple_key_context && a.allow_block {
                return Some(s);
            }
        }
        if (style.is_none() || style == Some('\''))
            && a.allow_single_quoted
            && !(self.simple_key_context && a.multiline)
        {
            return Some('\'');
        }
        Some('"')
    }

    /// `Emitter.check_simple_key` for a mapping key.
    fn check_simple_key(node: &Node) -> bool {
        match node {
            Node::Scalar { tag, value, .. } => {
                let a = analyze_scalar(value);
                let length = prepare_tag(tag).chars().count() + value.len();
                length < 128 && !a.empty && !a.multiline
            }
            Node::Seq { tag, items } => prepare_tag(tag).chars().count() < 128 && items.is_empty(),
            Node::Map { tag, pairs } => prepare_tag(tag).chars().count() < 128 && pairs.is_empty(),
        }
    }

    fn expect_flow_sequence(&mut self, items: &[Node]) {
        self.write_indicator("[", true, true, false);
        self.flow_level += 1;
        self.increase_indent(true, false);
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.write_indicator(",", false, false, false);
            }
            if self.column > BEST_WIDTH {
                self.write_indent();
            }
            self.expect_node(item, false, true, false, false);
        }
        self.pop_indent();
        self.flow_level -= 1;
        self.write_indicator("]", false, false, false);
    }

    fn expect_flow_mapping(&mut self, pairs: &[(Node, Node)]) {
        self.write_indicator("{", true, true, false);
        self.flow_level += 1;
        self.increase_indent(true, false);
        for (i, (k, v)) in pairs.iter().enumerate() {
            if i > 0 {
                self.write_indicator(",", false, false, false);
            }
            if self.column > BEST_WIDTH {
                self.write_indent();
            }
            if Self::check_simple_key(k) {
                self.expect_node(k, false, false, true, true);
                self.write_indicator(":", false, false, false);
                self.expect_node(v, false, false, true, false);
            } else {
                self.write_indicator("?", true, false, false);
                self.expect_node(k, false, false, true, false);
                if self.column > BEST_WIDTH {
                    self.write_indent();
                }
                self.write_indicator(":", true, false, false);
                self.expect_node(v, false, false, true, false);
            }
        }
        self.pop_indent();
        self.flow_level -= 1;
        self.write_indicator("}", false, false, false);
    }

    fn expect_block_sequence(&mut self, items: &[Node]) {
        let indentless = self.mapping_context && !self.indention;
        self.increase_indent(false, indentless);
        for item in items {
            self.write_indent();
            self.write_indicator("-", true, false, true);
            self.expect_node(item, false, true, false, false);
        }
        self.pop_indent();
    }

    fn expect_block_mapping(&mut self, pairs: &[(Node, Node)]) {
        self.increase_indent(false, false);
        for (k, v) in pairs {
            self.write_indent();
            if Self::check_simple_key(k) {
                self.expect_node(k, false, false, true, true);
                self.write_indicator(":", false, false, false);
                self.expect_node(v, false, false, true, false);
            } else {
                self.write_indicator("?", true, false, true);
                self.expect_node(k, false, false, true, false);
                self.write_indent();
                self.write_indicator(":", true, false, true);
                self.expect_node(v, false, false, true, false);
            }
        }
        self.pop_indent();
    }

    // ── writers ──────────────────────────────────────────────────────────

    fn write(&mut self, data: &str) {
        self.out.push_str(data);
    }

    fn write_chars(&mut self, data: &[char]) {
        self.column += data.len();
        self.out.extend(data.iter());
    }

    fn write_indicator(
        &mut self,
        indicator: &str,
        need_whitespace: bool,
        whitespace: bool,
        indention: bool,
    ) {
        let data = if self.whitespace || !need_whitespace {
            indicator.to_string()
        } else {
            format!(" {indicator}")
        };
        self.whitespace = whitespace;
        self.indention = self.indention && indention;
        self.column += data.chars().count();
        self.open_ended = false;
        self.write(&data);
    }

    fn write_indent(&mut self) {
        let indent = self.indent.unwrap_or(0).max(0) as usize;
        if !self.indention || self.column > indent || (self.column == indent && !self.whitespace) {
            self.write_line_break(None);
        }
        if self.column < indent {
            self.whitespace = true;
            let pad = " ".repeat(indent - self.column);
            self.column = indent;
            self.write(&pad);
        }
    }

    fn write_line_break(&mut self, data: Option<char>) {
        self.whitespace = true;
        self.indention = true;
        self.column = 0;
        self.out.push(data.unwrap_or('\n'));
    }

    fn write_breaks(&mut self, text: &[char]) {
        for &br in text {
            if br == '\n' {
                self.write_line_break(None);
            } else {
                self.write_line_break(Some(br));
            }
        }
    }

    fn write_plain(&mut self, text: &[char], split: bool) {
        if self.root_context {
            self.open_ended = true;
        }
        if text.is_empty() {
            return;
        }
        if !self.whitespace {
            self.column += 1;
            self.write(" ");
        }
        self.whitespace = false;
        self.indention = false;
        let mut spaces = false;
        let mut breaks = false;
        let (mut start, mut end) = (0usize, 0usize);
        while end <= text.len() {
            let ch = text.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.column > BEST_WIDTH && split {
                        self.write_indent();
                        self.whitespace = false;
                        self.indention = false;
                    } else {
                        self.write_chars(&text[start..end]);
                    }
                    start = end;
                }
            } else if breaks {
                if !ch.is_some_and(is_break) {
                    if text[start] == '\n' {
                        self.write_line_break(None);
                    }
                    self.write_breaks(&text[start..end]);
                    self.write_indent();
                    self.whitespace = false;
                    self.indention = false;
                    start = end;
                }
            } else if ch.is_none_or(|c| c == ' ' || is_break(c)) {
                self.write_chars(&text[start..end]);
                start = end;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = is_break(c);
            }
            end += 1;
        }
    }

    fn write_single_quoted(&mut self, text: &[char], split: bool) {
        self.write_indicator("'", true, false, false);
        let mut spaces = false;
        let mut breaks = false;
        let (mut start, mut end) = (0usize, 0usize);
        while end <= text.len() {
            let ch = text.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end
                        && self.column > BEST_WIDTH
                        && split
                        && start != 0
                        && end != text.len()
                    {
                        self.write_indent();
                    } else {
                        self.write_chars(&text[start..end]);
                    }
                    start = end;
                }
            } else if breaks {
                if !ch.is_some_and(is_break) {
                    if text[start] == '\n' {
                        self.write_line_break(None);
                    }
                    self.write_breaks(&text[start..end]);
                    self.write_indent();
                    start = end;
                }
            } else if ch.is_none_or(|c| c == ' ' || is_break(c) || c == '\'') && start < end {
                self.write_chars(&text[start..end]);
                start = end;
            }
            if ch == Some('\'') {
                self.column += 2;
                self.write("''");
                start = end + 1;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = is_break(c);
            }
            end += 1;
        }
        self.write_indicator("'", false, false, false);
    }

    fn write_double_quoted(&mut self, text: &[char], split: bool) {
        self.write_indicator("\"", true, false, false);
        let (mut start, mut end) = (0usize, 0usize);
        while end <= text.len() {
            let ch = text.get(end).copied();
            let needs_escape = match ch {
                None => true,
                Some(c) => {
                    "\"\\\u{85}\u{2028}\u{2029}\u{FEFF}".contains(c)
                        || !(('\x20'..='\x7E').contains(&c)
                            || ('\u{A0}'..='\u{D7FF}').contains(&c)
                            || ('\u{E000}'..='\u{FFFD}').contains(&c))
                }
            };
            if needs_escape {
                if start < end {
                    self.write_chars(&text[start..end]);
                    start = end;
                }
                if let Some(c) = ch {
                    let data = match double_quote_escape(c) {
                        Some(e) => format!("\\{e}"),
                        None if (c as u32) <= 0xFF => format!("\\x{:02X}", c as u32),
                        None if (c as u32) <= 0xFFFF => format!("\\u{:04X}", c as u32),
                        None => format!("\\U{:08X}", c as u32),
                    };
                    self.column += data.chars().count();
                    self.write(&data);
                    start = end + 1;
                }
            }
            if 0 < end
                && end + 1 < text.len()
                && (ch == Some(' ') || start >= end)
                && self.column as isize + end as isize - start as isize > BEST_WIDTH as isize
                && split
            {
                let mut data: String = if start < end {
                    text[start..end].iter().collect()
                } else {
                    String::new()
                };
                data.push('\\');
                if start < end {
                    start = end;
                }
                self.column += data.chars().count();
                self.write(&data);
                self.write_indent();
                self.whitespace = false;
                self.indention = false;
                if text[start] == ' ' {
                    self.column += 1;
                    self.write("\\");
                }
            }
            end += 1;
        }
        self.write_indicator("\"", false, false, false);
    }

    fn determine_block_hints(text: &[char]) -> String {
        let mut hints = String::new();
        if let (Some(&first), Some(&last)) = (text.first(), text.last()) {
            if first == ' ' || is_break(first) {
                hints.push_str(&BEST_INDENT.to_string());
            }
            if !is_break(last) {
                hints.push('-');
            } else if text.len() == 1 || is_break(text[text.len() - 2]) {
                hints.push('+');
            }
        }
        hints
    }

    fn write_literal(&mut self, text: &[char]) {
        let hints = Self::determine_block_hints(text);
        self.write_indicator(&format!("|{hints}"), true, false, false);
        if hints.ends_with('+') {
            self.open_ended = true;
        }
        self.write_line_break(None);
        let mut breaks = true;
        let (mut start, mut end) = (0usize, 0usize);
        while end <= text.len() {
            let ch = text.get(end).copied();
            if breaks {
                if !ch.is_some_and(is_break) {
                    self.write_breaks(&text[start..end]);
                    if ch.is_some() {
                        self.write_indent();
                    }
                    start = end;
                }
            } else if ch.is_none_or(is_break) {
                // write_literal does not advance `column` for the text.
                self.out.extend(text[start..end].iter());
                if ch.is_none() {
                    self.write_line_break(None);
                }
                start = end;
            }
            if let Some(c) = ch {
                breaks = is_break(c);
            }
            end += 1;
        }
    }
}

fn double_quote_escape(c: char) -> Option<char> {
    Some(match c {
        '\0' => '0',
        '\x07' => 'a',
        '\x08' => 'b',
        '\x09' => 't',
        '\x0A' => 'n',
        '\x0B' => 'v',
        '\x0C' => 'f',
        '\x0D' => 'r',
        '\x1B' => 'e',
        '"' => '"',
        '\\' => '\\',
        '\u{85}' => 'N',
        '\u{A0}' => '_',
        '\u{2028}' => 'L',
        '\u{2029}' => 'P',
        _ => return None,
    })
}
