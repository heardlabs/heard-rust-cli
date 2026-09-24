//! `yaml/reader.py` + `yaml/scanner.py`, ported line for line.
//!
//! The structure (and most of the names) follow PyYAML 6's pure-Python
//! scanner, because the point is to accept and reject EXACTLY what
//! `yaml.safe_load` does and to produce the same scalar text. Where Python
//! reads past the end of its buffer it would raise `IndexError`; every such
//! read is guarded by an earlier check in the original, and here a read past
//! the end simply sees the `'\0'` terminator.

use super::YamlError;
use std::collections::{BTreeMap, VecDeque};

/// Where in the stream a token starts or ends (`yaml.error.Mark`, minus the
/// snippet).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Mark {
    pub index: usize,
    pub line: usize,
    pub column: usize,
}

/// The value of a `%` directive.
#[derive(Clone, Debug)]
pub(crate) enum Directive {
    Yaml(u64, u64),
    Tag(String, String),
    Other,
}

#[derive(Clone, Debug)]
pub(crate) enum Tok {
    StreamStart,
    StreamEnd,
    Directive(String, Directive),
    DocumentStart,
    DocumentEnd,
    BlockSequenceStart,
    BlockMappingStart,
    BlockEnd,
    FlowSequenceStart,
    FlowMappingStart,
    FlowSequenceEnd,
    FlowMappingEnd,
    Key,
    Value,
    BlockEntry,
    FlowEntry,
    Alias(String),
    Anchor(String),
    /// `(handle, suffix)`.
    Tag(Option<String>, String),
    Scalar {
        value: String,
        plain: bool,
    },
}

impl Tok {
    /// `Token.id`, for error messages.
    pub(crate) fn id(&self) -> &'static str {
        match self {
            Tok::StreamStart => "<stream start>",
            Tok::StreamEnd => "<stream end>",
            Tok::Directive(..) => "<directive>",
            Tok::DocumentStart => "<document start>",
            Tok::DocumentEnd => "<document end>",
            Tok::BlockSequenceStart => "<block sequence start>",
            Tok::BlockMappingStart => "<block mapping start>",
            Tok::BlockEnd => "<block end>",
            Tok::FlowSequenceStart => "[",
            Tok::FlowMappingStart => "{",
            Tok::FlowSequenceEnd => "]",
            Tok::FlowMappingEnd => "}",
            Tok::Key => "?",
            Tok::Value => ":",
            Tok::BlockEntry => "-",
            Tok::FlowEntry => ",",
            Tok::Alias(_) => "<alias>",
            Tok::Anchor(_) => "<anchor>",
            Tok::Tag(..) => "<tag>",
            Tok::Scalar { .. } => "<scalar>",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Token {
    pub kind: Tok,
    pub start: Mark,
    pub end: Mark,
}

struct SimpleKey {
    token_number: usize,
    required: bool,
    index: usize,
    line: usize,
    column: usize,
    mark: Mark,
}

/// Any of these characters (Python's `'\0 \t\r\n\x85\u2028\u2029'`).
fn in_set(ch: char, set: &str) -> bool {
    set.contains(ch)
}

const BLANKZ: &str = "\0 \t\r\n\u{85}\u{2028}\u{2029}";
const BREAKZ: &str = "\0\r\n\u{85}\u{2028}\u{2029}";
const BREAKS: &str = "\r\n\u{85}\u{2028}\u{2029}";
/// `'\0 \r\n\x85\u2028\u2029'` — BLANKZ without the tab.
const SPACEZ: &str = "\0 \r\n\u{85}\u{2028}\u{2029}";

fn is_word(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'
}

fn err(context: Option<&str>, problem: String, mark: Mark) -> YamlError {
    YamlError::new(context, problem, mark)
}

/// `Reader.NON_PRINTABLE` — a character PyYAML refuses anywhere in a stream.
pub(crate) fn is_printable(ch: char) -> bool {
    matches!(ch,
        '\u{09}' | '\u{0A}' | '\u{0D}' | '\u{20}'..='\u{7E}' | '\u{85}'
        | '\u{A0}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}')
}

pub(crate) struct Scanner {
    buf: Vec<char>,
    pointer: usize,
    line: usize,
    column: usize,

    done: bool,
    flow_level: usize,
    tokens: VecDeque<Token>,
    tokens_taken: usize,
    indent: isize,
    indents: Vec<isize>,
    allow_simple_key: bool,
    possible_simple_keys: BTreeMap<usize, SimpleKey>,
    /// A surrogate escape was decoded (see `scan_flow_scalar_non_spaces`).
    pub(crate) saw_surrogate: bool,
}

type R<T> = Result<T, YamlError>;

impl Scanner {
    /// `Reader(stream)` + `Scanner.__init__`. `text` is what Python's
    /// text-mode `open()` hands PyYAML: already decoded, newlines already
    /// universal.
    pub(crate) fn new(text: &str) -> R<Self> {
        if let Some((pos, ch)) = text.chars().enumerate().find(|(_, c)| !is_printable(*c)) {
            return Err(YamlError::reader(format!(
                "unacceptable character #x{:04x}: special characters are not allowed\n  in \"<file>\", position {pos}",
                ch as u32
            )));
        }
        let mut buf: Vec<char> = text.chars().collect();
        buf.push('\0');
        let mut s = Scanner {
            buf,
            pointer: 0,
            line: 0,
            column: 0,
            done: false,
            flow_level: 0,
            tokens: VecDeque::new(),
            tokens_taken: 0,
            indent: -1,
            indents: Vec::new(),
            allow_simple_key: true,
            possible_simple_keys: BTreeMap::new(),
            saw_surrogate: false,
        };
        let mark = s.mark();
        s.tokens.push_back(Token {
            kind: Tok::StreamStart,
            start: mark,
            end: mark,
        });
        Ok(s)
    }

    // ── reader ───────────────────────────────────────────────────────────

    fn peek(&self, k: usize) -> char {
        self.buf.get(self.pointer + k).copied().unwrap_or('\0')
    }

    fn ch(&self) -> char {
        self.peek(0)
    }

    fn prefix(&self, length: usize) -> String {
        let end = (self.pointer + length).min(self.buf.len());
        self.buf[self.pointer..end].iter().collect()
    }

    fn forward(&mut self, length: usize) {
        for _ in 0..length {
            let ch = self.buf[self.pointer];
            self.pointer += 1;
            if in_set(ch, "\n\u{85}\u{2028}\u{2029}") || (ch == '\r' && self.ch() != '\n') {
                self.line += 1;
                self.column = 0;
            } else if ch != '\u{FEFF}' {
                self.column += 1;
            }
        }
    }

    fn mark(&self) -> Mark {
        Mark {
            index: self.pointer,
            line: self.line,
            column: self.column,
        }
    }

    // ── public API ───────────────────────────────────────────────────────

    pub(crate) fn check(&mut self, pred: impl Fn(&Tok) -> bool) -> R<bool> {
        Ok(self.peek_token()?.is_some_and(|t| pred(&t.kind)))
    }

    pub(crate) fn peek_token(&mut self) -> R<Option<&Token>> {
        while self.need_more_tokens()? {
            self.fetch_more_tokens()?;
        }
        Ok(self.tokens.front())
    }

    pub(crate) fn get_token(&mut self) -> R<Option<Token>> {
        while self.need_more_tokens()? {
            self.fetch_more_tokens()?;
        }
        let t = self.tokens.pop_front();
        if t.is_some() {
            self.tokens_taken += 1;
        }
        Ok(t)
    }

    fn need_more_tokens(&mut self) -> R<bool> {
        if self.done {
            return Ok(false);
        }
        if self.tokens.is_empty() {
            return Ok(true);
        }
        self.stale_possible_simple_keys()?;
        Ok(self.next_possible_simple_key() == Some(self.tokens_taken))
    }

    fn fetch_more_tokens(&mut self) -> R<()> {
        self.scan_to_next_token();
        self.stale_possible_simple_keys()?;
        self.unwind_indent(self.column as isize);
        let ch = self.ch();
        if ch == '\0' {
            return self.fetch_stream_end();
        }
        if ch == '%' && self.column == 0 {
            return self.fetch_directive();
        }
        if ch == '-' && self.check_document_indicator("---") {
            return self.fetch_document_indicator(Tok::DocumentStart);
        }
        if ch == '.' && self.check_document_indicator("...") {
            return self.fetch_document_indicator(Tok::DocumentEnd);
        }
        match ch {
            '[' => return self.fetch_flow_collection_start(Tok::FlowSequenceStart),
            '{' => return self.fetch_flow_collection_start(Tok::FlowMappingStart),
            ']' => return self.fetch_flow_collection_end(Tok::FlowSequenceEnd),
            '}' => return self.fetch_flow_collection_end(Tok::FlowMappingEnd),
            ',' => return self.fetch_flow_entry(),
            _ => {}
        }
        if ch == '-' && in_set(self.peek(1), BLANKZ) {
            return self.fetch_block_entry();
        }
        if ch == '?' && (self.flow_level > 0 || in_set(self.peek(1), BLANKZ)) {
            return self.fetch_key();
        }
        if ch == ':' && (self.flow_level > 0 || in_set(self.peek(1), BLANKZ)) {
            return self.fetch_value();
        }
        match ch {
            '*' => return self.fetch_anchor(true),
            '&' => return self.fetch_anchor(false),
            '!' => return self.fetch_tag(),
            '|' if self.flow_level == 0 => return self.fetch_block_scalar('|'),
            '>' if self.flow_level == 0 => return self.fetch_block_scalar('>'),
            '\'' => return self.fetch_flow_scalar('\''),
            '"' => return self.fetch_flow_scalar('"'),
            _ => {}
        }
        if self.check_plain() {
            return self.fetch_plain();
        }
        Err(err(
            Some("while scanning for the next token"),
            format!(
                "found character {} that cannot start any token",
                py_repr_char(ch)
            ),
            self.mark(),
        ))
    }

    // ── simple keys ──────────────────────────────────────────────────────

    fn next_possible_simple_key(&self) -> Option<usize> {
        self.possible_simple_keys
            .values()
            .map(|k| k.token_number)
            .min()
    }

    fn stale_possible_simple_keys(&mut self) -> R<()> {
        let levels: Vec<usize> = self.possible_simple_keys.keys().copied().collect();
        for level in levels {
            let key = &self.possible_simple_keys[&level];
            if key.line != self.line || self.pointer - key.index > 1024 {
                if key.required {
                    return Err(YamlError::marked(
                        Some("while scanning a simple key"),
                        Some(key.mark),
                        "could not find expected ':'".into(),
                        self.mark(),
                    ));
                }
                self.possible_simple_keys.remove(&level);
            }
        }
        Ok(())
    }

    fn save_possible_simple_key(&mut self) -> R<()> {
        let required = self.flow_level == 0 && self.indent == self.column as isize;
        if self.allow_simple_key {
            self.remove_possible_simple_key()?;
            let token_number = self.tokens_taken + self.tokens.len();
            let key = SimpleKey {
                token_number,
                required,
                index: self.pointer,
                line: self.line,
                column: self.column,
                mark: self.mark(),
            };
            self.possible_simple_keys.insert(self.flow_level, key);
        }
        Ok(())
    }

    fn remove_possible_simple_key(&mut self) -> R<()> {
        if let Some(key) = self.possible_simple_keys.get(&self.flow_level) {
            if key.required {
                return Err(YamlError::marked(
                    Some("while scanning a simple key"),
                    Some(key.mark),
                    "could not find expected ':'".into(),
                    self.mark(),
                ));
            }
            self.possible_simple_keys.remove(&self.flow_level);
        }
        Ok(())
    }

    // ── indentation ──────────────────────────────────────────────────────

    fn unwind_indent(&mut self, column: isize) {
        if self.flow_level > 0 {
            return;
        }
        while self.indent > column {
            let mark = self.mark();
            self.indent = self.indents.pop().unwrap_or(-1);
            self.push(Tok::BlockEnd, mark, mark);
        }
    }

    fn add_indent(&mut self, column: usize) -> bool {
        let column = column as isize;
        if self.indent < column {
            self.indents.push(self.indent);
            self.indent = column;
            return true;
        }
        false
    }

    fn push(&mut self, kind: Tok, start: Mark, end: Mark) {
        self.tokens.push_back(Token { kind, start, end });
    }

    // ── fetchers ─────────────────────────────────────────────────────────

    fn fetch_stream_end(&mut self) -> R<()> {
        self.unwind_indent(-1);
        self.remove_possible_simple_key()?;
        self.allow_simple_key = false;
        self.possible_simple_keys.clear();
        let mark = self.mark();
        self.push(Tok::StreamEnd, mark, mark);
        self.done = true;
        Ok(())
    }

    fn fetch_directive(&mut self) -> R<()> {
        self.unwind_indent(-1);
        self.remove_possible_simple_key()?;
        self.allow_simple_key = false;
        let tok = self.scan_directive()?;
        self.tokens.push_back(tok);
        Ok(())
    }

    fn check_document_indicator(&self, which: &str) -> bool {
        self.column == 0 && self.prefix(3) == which && in_set(self.peek(3), BLANKZ)
    }

    fn fetch_document_indicator(&mut self, kind: Tok) -> R<()> {
        self.unwind_indent(-1);
        self.remove_possible_simple_key()?;
        self.allow_simple_key = false;
        let start = self.mark();
        self.forward(3);
        let end = self.mark();
        self.push(kind, start, end);
        Ok(())
    }

    fn fetch_flow_collection_start(&mut self, kind: Tok) -> R<()> {
        self.save_possible_simple_key()?;
        self.flow_level += 1;
        self.allow_simple_key = true;
        let start = self.mark();
        self.forward(1);
        let end = self.mark();
        self.push(kind, start, end);
        Ok(())
    }

    fn fetch_flow_collection_end(&mut self, kind: Tok) -> R<()> {
        self.remove_possible_simple_key()?;
        // Python lets flow_level go negative on a stray `]`; the parser then
        // rejects the token. Saturating keeps the same observable outcome.
        self.flow_level = self.flow_level.saturating_sub(1);
        self.allow_simple_key = false;
        let start = self.mark();
        self.forward(1);
        let end = self.mark();
        self.push(kind, start, end);
        Ok(())
    }

    fn fetch_flow_entry(&mut self) -> R<()> {
        self.allow_simple_key = true;
        self.remove_possible_simple_key()?;
        let start = self.mark();
        self.forward(1);
        let end = self.mark();
        self.push(Tok::FlowEntry, start, end);
        Ok(())
    }

    fn fetch_block_entry(&mut self) -> R<()> {
        if self.flow_level == 0 {
            if !self.allow_simple_key {
                return Err(err(
                    None,
                    "sequence entries are not allowed here".into(),
                    self.mark(),
                ));
            }
            if self.add_indent(self.column) {
                let mark = self.mark();
                self.push(Tok::BlockSequenceStart, mark, mark);
            }
        }
        self.allow_simple_key = true;
        self.remove_possible_simple_key()?;
        let start = self.mark();
        self.forward(1);
        let end = self.mark();
        self.push(Tok::BlockEntry, start, end);
        Ok(())
    }

    fn fetch_key(&mut self) -> R<()> {
        if self.flow_level == 0 {
            if !self.allow_simple_key {
                return Err(err(
                    None,
                    "mapping keys are not allowed here".into(),
                    self.mark(),
                ));
            }
            if self.add_indent(self.column) {
                let mark = self.mark();
                self.push(Tok::BlockMappingStart, mark, mark);
            }
        }
        self.allow_simple_key = self.flow_level == 0;
        self.remove_possible_simple_key()?;
        let start = self.mark();
        self.forward(1);
        let end = self.mark();
        self.push(Tok::Key, start, end);
        Ok(())
    }

    fn fetch_value(&mut self) -> R<()> {
        if let Some(key) = self.possible_simple_keys.remove(&self.flow_level) {
            let at = key.token_number - self.tokens_taken;
            self.tokens.insert(
                at,
                Token {
                    kind: Tok::Key,
                    start: key.mark,
                    end: key.mark,
                },
            );
            if self.flow_level == 0 && self.add_indent(key.column) {
                self.tokens.insert(
                    at,
                    Token {
                        kind: Tok::BlockMappingStart,
                        start: key.mark,
                        end: key.mark,
                    },
                );
            }
            self.allow_simple_key = false;
        } else {
            if self.flow_level == 0 {
                if !self.allow_simple_key {
                    return Err(err(
                        None,
                        "mapping values are not allowed here".into(),
                        self.mark(),
                    ));
                }
                if self.add_indent(self.column) {
                    let mark = self.mark();
                    self.push(Tok::BlockMappingStart, mark, mark);
                }
            }
            self.allow_simple_key = self.flow_level == 0;
            self.remove_possible_simple_key()?;
        }
        let start = self.mark();
        self.forward(1);
        let end = self.mark();
        self.push(Tok::Value, start, end);
        Ok(())
    }

    fn fetch_anchor(&mut self, alias: bool) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let tok = self.scan_anchor(alias)?;
        self.tokens.push_back(tok);
        Ok(())
    }

    fn fetch_tag(&mut self) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let tok = self.scan_tag()?;
        self.tokens.push_back(tok);
        Ok(())
    }

    fn fetch_block_scalar(&mut self, style: char) -> R<()> {
        self.allow_simple_key = true;
        self.remove_possible_simple_key()?;
        let tok = self.scan_block_scalar(style)?;
        self.tokens.push_back(tok);
        Ok(())
    }

    fn fetch_flow_scalar(&mut self, style: char) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let tok = self.scan_flow_scalar(style)?;
        self.tokens.push_back(tok);
        Ok(())
    }

    fn fetch_plain(&mut self) -> R<()> {
        self.save_possible_simple_key()?;
        self.allow_simple_key = false;
        let tok = self.scan_plain()?;
        self.tokens.push_back(tok);
        Ok(())
    }

    fn check_plain(&self) -> bool {
        let ch = self.ch();
        !in_set(ch, "\0 \t\r\n\u{85}\u{2028}\u{2029}-?:,[]{}#&*!|>'\"%@`")
            || (!in_set(self.peek(1), BLANKZ)
                && (ch == '-' || (self.flow_level == 0 && in_set(ch, "?:"))))
    }

    // ── scanners ─────────────────────────────────────────────────────────

    fn scan_to_next_token(&mut self) {
        if self.pointer == 0 && self.ch() == '\u{FEFF}' {
            self.forward(1);
        }
        loop {
            while self.ch() == ' ' {
                self.forward(1);
            }
            if self.ch() == '#' {
                while !in_set(self.ch(), BREAKZ) {
                    self.forward(1);
                }
            }
            if !self.scan_line_break().is_empty() {
                if self.flow_level == 0 {
                    self.allow_simple_key = true;
                }
            } else {
                break;
            }
        }
    }

    fn scan_directive(&mut self) -> R<Token> {
        let start = self.mark();
        self.forward(1);
        let name = self.scan_directive_name(start)?;
        let value;
        let end;
        if name == "YAML" {
            value = self.scan_yaml_directive_value(start)?;
            end = self.mark();
        } else if name == "TAG" {
            value = self.scan_tag_directive_value(start)?;
            end = self.mark();
        } else {
            end = self.mark();
            while !in_set(self.ch(), BREAKZ) {
                self.forward(1);
            }
            value = Directive::Other;
        }
        self.scan_directive_ignored_line(start)?;
        Ok(Token {
            kind: Tok::Directive(name, value),
            start,
            end,
        })
    }

    fn scan_directive_name(&mut self, start: Mark) -> R<String> {
        let mut length = 0;
        while is_word(self.peek(length)) {
            length += 1;
        }
        if length == 0 {
            return Err(YamlError::marked(
                Some("while scanning a directive"),
                Some(start),
                format!(
                    "expected alphabetic or numeric character, but found {}",
                    py_repr_char(self.peek(length))
                ),
                self.mark(),
            ));
        }
        let value = self.prefix(length);
        self.forward(length);
        if !in_set(self.ch(), SPACEZ) {
            return Err(YamlError::marked(
                Some("while scanning a directive"),
                Some(start),
                format!(
                    "expected alphabetic or numeric character, but found {}",
                    py_repr_char(self.ch())
                ),
                self.mark(),
            ));
        }
        Ok(value)
    }

    fn scan_yaml_directive_value(&mut self, start: Mark) -> R<Directive> {
        while self.ch() == ' ' {
            self.forward(1);
        }
        let major = self.scan_yaml_directive_number(start)?;
        if self.ch() != '.' {
            return Err(YamlError::marked(
                Some("while scanning a directive"),
                Some(start),
                format!(
                    "expected a digit or '.', but found {}",
                    py_repr_char(self.ch())
                ),
                self.mark(),
            ));
        }
        self.forward(1);
        let minor = self.scan_yaml_directive_number(start)?;
        if !in_set(self.ch(), SPACEZ) {
            return Err(YamlError::marked(
                Some("while scanning a directive"),
                Some(start),
                format!(
                    "expected a digit or ' ', but found {}",
                    py_repr_char(self.ch())
                ),
                self.mark(),
            ));
        }
        Ok(Directive::Yaml(major, minor))
    }

    fn scan_yaml_directive_number(&mut self, start: Mark) -> R<u64> {
        if !self.ch().is_ascii_digit() {
            return Err(YamlError::marked(
                Some("while scanning a directive"),
                Some(start),
                format!("expected a digit, but found {}", py_repr_char(self.ch())),
                self.mark(),
            ));
        }
        let mut length = 0;
        while self.peek(length).is_ascii_digit() {
            length += 1;
        }
        // Python's int() is unbounded; anything past u64 is "not 1" anyway.
        let digits = self.prefix(length);
        let trimmed = digits.trim_start_matches('0');
        let value = if trimmed.is_empty() {
            0
        } else {
            trimmed.parse::<u64>().unwrap_or(u64::MAX)
        };
        self.forward(length);
        Ok(value)
    }

    fn scan_tag_directive_value(&mut self, start: Mark) -> R<Directive> {
        while self.ch() == ' ' {
            self.forward(1);
        }
        let handle = self.scan_tag_handle("directive", start)?;
        if self.ch() != ' ' {
            return Err(YamlError::marked(
                Some("while scanning a directive"),
                Some(start),
                format!("expected ' ', but found {}", py_repr_char(self.ch())),
                self.mark(),
            ));
        }
        while self.ch() == ' ' {
            self.forward(1);
        }
        let prefix = self.scan_tag_uri("directive", start)?;
        if !in_set(self.ch(), SPACEZ) {
            return Err(YamlError::marked(
                Some("while scanning a directive"),
                Some(start),
                format!("expected ' ', but found {}", py_repr_char(self.ch())),
                self.mark(),
            ));
        }
        Ok(Directive::Tag(handle, prefix))
    }

    fn scan_directive_ignored_line(&mut self, start: Mark) -> R<()> {
        while self.ch() == ' ' {
            self.forward(1);
        }
        if self.ch() == '#' {
            while !in_set(self.ch(), BREAKZ) {
                self.forward(1);
            }
        }
        if !in_set(self.ch(), BREAKZ) {
            return Err(YamlError::marked(
                Some("while scanning a directive"),
                Some(start),
                format!(
                    "expected a comment or a line break, but found {}",
                    py_repr_char(self.ch())
                ),
                self.mark(),
            ));
        }
        self.scan_line_break();
        Ok(())
    }

    fn scan_anchor(&mut self, alias: bool) -> R<Token> {
        let start = self.mark();
        let name = if alias { "alias" } else { "anchor" };
        self.forward(1);
        let mut length = 0;
        while is_word(self.peek(length)) {
            length += 1;
        }
        if length == 0 {
            return Err(YamlError::marked(
                Some(&format!("while scanning an {name}")),
                Some(start),
                format!(
                    "expected alphabetic or numeric character, but found {}",
                    py_repr_char(self.peek(length))
                ),
                self.mark(),
            ));
        }
        let value = self.prefix(length);
        self.forward(length);
        if !in_set(self.ch(), "\0 \t\r\n\u{85}\u{2028}\u{2029}?:,]}%@`") {
            return Err(YamlError::marked(
                Some(&format!("while scanning an {name}")),
                Some(start),
                format!(
                    "expected alphabetic or numeric character, but found {}",
                    py_repr_char(self.ch())
                ),
                self.mark(),
            ));
        }
        let end = self.mark();
        let kind = if alias {
            Tok::Alias(value)
        } else {
            Tok::Anchor(value)
        };
        Ok(Token { kind, start, end })
    }

    fn scan_tag(&mut self) -> R<Token> {
        let start = self.mark();
        let mut ch = self.peek(1);
        let handle;
        let suffix;
        if ch == '<' {
            handle = None;
            self.forward(2);
            suffix = self.scan_tag_uri("tag", start)?;
            if self.ch() != '>' {
                return Err(YamlError::marked(
                    Some("while parsing a tag"),
                    Some(start),
                    format!("expected '>', but found {}", py_repr_char(self.ch())),
                    self.mark(),
                ));
            }
            self.forward(1);
        } else if in_set(ch, BLANKZ) {
            handle = None;
            suffix = "!".to_string();
            self.forward(1);
        } else {
            let mut length = 1;
            let mut use_handle = false;
            while !in_set(ch, SPACEZ) {
                if ch == '!' {
                    use_handle = true;
                    break;
                }
                length += 1;
                ch = self.peek(length);
            }
            if use_handle {
                handle = Some(self.scan_tag_handle("tag", start)?);
            } else {
                handle = Some("!".to_string());
                self.forward(1);
            }
            suffix = self.scan_tag_uri("tag", start)?;
        }
        if !in_set(self.ch(), SPACEZ) {
            return Err(YamlError::marked(
                Some("while scanning a tag"),
                Some(start),
                format!("expected ' ', but found {}", py_repr_char(self.ch())),
                self.mark(),
            ));
        }
        let end = self.mark();
        Ok(Token {
            kind: Tok::Tag(handle, suffix),
            start,
            end,
        })
    }

    fn scan_block_scalar(&mut self, style: char) -> R<Token> {
        let folded = style == '>';
        let mut chunks = String::new();
        let start = self.mark();
        self.forward(1);
        let (chomping, increment) = self.scan_block_scalar_indicators(start)?;
        self.scan_block_scalar_ignored_line(start)?;
        let mut min_indent = self.indent + 1;
        if min_indent < 1 {
            min_indent = 1;
        }
        let min_indent = min_indent as usize;
        let indent;
        let mut breaks;
        let mut end;
        match increment {
            None => {
                let (b, max_indent, e) = self.scan_block_scalar_indentation();
                breaks = b;
                end = e;
                indent = min_indent.max(max_indent);
            }
            Some(inc) => {
                indent = min_indent + inc - 1;
                let (b, e) = self.scan_block_scalar_breaks(indent);
                breaks = b;
                end = e;
            }
        }
        let mut line_break = String::new();
        while self.column == indent && self.ch() != '\0' {
            for b in &breaks {
                chunks.push_str(b);
            }
            let leading_non_space = !in_set(self.ch(), " \t");
            let mut length = 0;
            while !in_set(self.peek(length), BREAKZ) {
                length += 1;
            }
            chunks.push_str(&self.prefix(length));
            self.forward(length);
            line_break = self.scan_line_break();
            let (b, e) = self.scan_block_scalar_breaks(indent);
            breaks = b;
            end = e;
            if self.column == indent && self.ch() != '\0' {
                if folded && line_break == "\n" && leading_non_space && !in_set(self.ch(), " \t") {
                    if breaks.is_empty() {
                        chunks.push(' ');
                    }
                } else {
                    chunks.push_str(&line_break);
                }
            } else {
                break;
            }
        }
        if chomping != Some(false) {
            chunks.push_str(&line_break);
        }
        if chomping == Some(true) {
            for b in &breaks {
                chunks.push_str(b);
            }
        }
        Ok(Token {
            kind: Tok::Scalar {
                value: chunks,
                plain: false,
            },
            start,
            end,
        })
    }

    fn scan_block_scalar_indicators(&mut self, start: Mark) -> R<(Option<bool>, Option<usize>)> {
        let mut chomping = None;
        let mut increment = None;
        let zero = |s: &Self| {
            YamlError::marked(
                Some("while scanning a block scalar"),
                Some(start),
                "expected indentation indicator in the range 1-9, but found 0".into(),
                s.mark(),
            )
        };
        let mut ch = self.ch();
        if ch == '+' || ch == '-' {
            chomping = Some(ch == '+');
            self.forward(1);
            ch = self.ch();
            if ch.is_ascii_digit() {
                let inc = ch as usize - '0' as usize;
                if inc == 0 {
                    return Err(zero(self));
                }
                increment = Some(inc);
                self.forward(1);
            }
        } else if ch.is_ascii_digit() {
            let inc = ch as usize - '0' as usize;
            if inc == 0 {
                return Err(zero(self));
            }
            increment = Some(inc);
            self.forward(1);
            ch = self.ch();
            if ch == '+' || ch == '-' {
                chomping = Some(ch == '+');
                self.forward(1);
            }
        }
        if !in_set(self.ch(), SPACEZ) {
            return Err(YamlError::marked(
                Some("while scanning a block scalar"),
                Some(start),
                format!(
                    "expected chomping or indentation indicators, but found {}",
                    py_repr_char(self.ch())
                ),
                self.mark(),
            ));
        }
        Ok((chomping, increment))
    }

    fn scan_block_scalar_ignored_line(&mut self, start: Mark) -> R<()> {
        while self.ch() == ' ' {
            self.forward(1);
        }
        if self.ch() == '#' {
            while !in_set(self.ch(), BREAKZ) {
                self.forward(1);
            }
        }
        if !in_set(self.ch(), BREAKZ) {
            return Err(YamlError::marked(
                Some("while scanning a block scalar"),
                Some(start),
                format!(
                    "expected a comment or a line break, but found {}",
                    py_repr_char(self.ch())
                ),
                self.mark(),
            ));
        }
        self.scan_line_break();
        Ok(())
    }

    fn scan_block_scalar_indentation(&mut self) -> (Vec<String>, usize, Mark) {
        let mut chunks = Vec::new();
        let mut max_indent = 0;
        let mut end = self.mark();
        while in_set(self.ch(), " \r\n\u{85}\u{2028}\u{2029}") {
            if self.ch() != ' ' {
                chunks.push(self.scan_line_break());
                end = self.mark();
            } else {
                self.forward(1);
                if self.column > max_indent {
                    max_indent = self.column;
                }
            }
        }
        (chunks, max_indent, end)
    }

    fn scan_block_scalar_breaks(&mut self, indent: usize) -> (Vec<String>, Mark) {
        let mut chunks = Vec::new();
        let mut end = self.mark();
        while self.column < indent && self.ch() == ' ' {
            self.forward(1);
        }
        while in_set(self.ch(), BREAKS) {
            chunks.push(self.scan_line_break());
            end = self.mark();
            while self.column < indent && self.ch() == ' ' {
                self.forward(1);
            }
        }
        (chunks, end)
    }

    fn scan_flow_scalar(&mut self, style: char) -> R<Token> {
        let double = style == '"';
        let mut chunks = String::new();
        let start = self.mark();
        let quote = self.ch();
        self.forward(1);
        self.scan_flow_scalar_non_spaces(double, start, &mut chunks)?;
        while self.ch() != quote {
            self.scan_flow_scalar_spaces(double, start, &mut chunks)?;
            self.scan_flow_scalar_non_spaces(double, start, &mut chunks)?;
        }
        self.forward(1);
        let end = self.mark();
        Ok(Token {
            kind: Tok::Scalar {
                value: chunks,
                plain: false,
            },
            start,
            end,
        })
    }

    fn scan_flow_scalar_non_spaces(
        &mut self,
        double: bool,
        start: Mark,
        chunks: &mut String,
    ) -> R<()> {
        loop {
            let mut length = 0;
            while !in_set(self.peek(length), "'\"\\\0 \t\r\n\u{85}\u{2028}\u{2029}") {
                length += 1;
            }
            if length > 0 {
                chunks.push_str(&self.prefix(length));
                self.forward(length);
            }
            let ch = self.ch();
            if !double && ch == '\'' && self.peek(1) == '\'' {
                chunks.push('\'');
                self.forward(2);
            } else if (double && ch == '\'') || (!double && (ch == '"' || ch == '\\')) {
                chunks.push(ch);
                self.forward(1);
            } else if double && ch == '\\' {
                self.forward(1);
                let ch = self.ch();
                if let Some(rep) = escape_replacement(ch) {
                    chunks.push(rep);
                    self.forward(1);
                } else if let Some(length) = escape_code_len(ch) {
                    self.forward(1);
                    for k in 0..length {
                        if !self.peek(k).is_ascii_hexdigit() {
                            return Err(YamlError::marked(
                                Some("while scanning a double-quoted scalar"),
                                Some(start),
                                format!(
                                    "expected escape sequence of {length} hexadecimal numbers, but found {}",
                                    py_repr_char(self.peek(k))
                                ),
                                self.mark(),
                            ));
                        }
                    }
                    let code = u32::from_str_radix(&self.prefix(length), 16).unwrap_or(u32::MAX);
                    match char::from_u32(code) {
                        Some(c) => chunks.push(c),
                        // Python's chr() accepts a lone surrogate and the load
                        // goes on (and may still fail as YAML). A Rust String
                        // cannot hold one, so remember it and refuse the
                        // document only if it otherwise loads.
                        None if (0xD800..=0xDFFF).contains(&code) => {
                            self.saw_surrogate = true;
                            chunks.push(char::REPLACEMENT_CHARACTER);
                        }
                        // chr() past 0x10FFFF: ValueError, right here.
                        None => {
                            return Err(YamlError::value(format!(
                                "chr() arg not in range(0x110000): {code:#x}"
                            )))
                        }
                    }
                    self.forward(length);
                } else if in_set(ch, BREAKS) {
                    self.scan_line_break();
                    self.scan_flow_scalar_breaks(double, start, chunks)?;
                } else {
                    return Err(YamlError::marked(
                        Some("while scanning a double-quoted scalar"),
                        Some(start),
                        format!("found unknown escape character {}", py_repr_char(ch)),
                        self.mark(),
                    ));
                }
            } else {
                return Ok(());
            }
        }
    }

    fn scan_flow_scalar_spaces(&mut self, double: bool, start: Mark, chunks: &mut String) -> R<()> {
        let mut length = 0;
        while in_set(self.peek(length), " \t") {
            length += 1;
        }
        let whitespaces = self.prefix(length);
        self.forward(length);
        let ch = self.ch();
        if ch == '\0' {
            return Err(YamlError::marked(
                Some("while scanning a quoted scalar"),
                Some(start),
                "found unexpected end of stream".into(),
                self.mark(),
            ));
        } else if in_set(ch, BREAKS) {
            let line_break = self.scan_line_break();
            let mut breaks = String::new();
            self.scan_flow_scalar_breaks(double, start, &mut breaks)?;
            if line_break != "\n" {
                chunks.push_str(&line_break);
            } else if breaks.is_empty() {
                chunks.push(' ');
            }
            chunks.push_str(&breaks);
        } else {
            chunks.push_str(&whitespaces);
        }
        Ok(())
    }

    fn scan_flow_scalar_breaks(
        &mut self,
        _double: bool,
        start: Mark,
        chunks: &mut String,
    ) -> R<()> {
        loop {
            let prefix = self.prefix(3);
            if (prefix == "---" || prefix == "...") && in_set(self.peek(3), BLANKZ) {
                return Err(YamlError::marked(
                    Some("while scanning a quoted scalar"),
                    Some(start),
                    "found unexpected document separator".into(),
                    self.mark(),
                ));
            }
            while in_set(self.ch(), " \t") {
                self.forward(1);
            }
            if in_set(self.ch(), BREAKS) {
                chunks.push_str(&self.scan_line_break());
            } else {
                return Ok(());
            }
        }
    }

    fn scan_plain(&mut self) -> R<Token> {
        let mut chunks = String::new();
        let start = self.mark();
        let mut end = start;
        let indent = self.indent + 1;
        let mut spaces: Option<String> = Some(String::new());
        loop {
            let mut length = 0;
            if self.ch() == '#' {
                break;
            }
            loop {
                let ch = self.peek(length);
                if in_set(ch, BLANKZ)
                    || (ch == ':'
                        && (in_set(self.peek(length + 1), BLANKZ)
                            || (self.flow_level > 0 && in_set(self.peek(length + 1), ",[]{}"))))
                    || (self.flow_level > 0 && in_set(ch, ",?[]{}"))
                {
                    break;
                }
                length += 1;
            }
            if length == 0 {
                break;
            }
            self.allow_simple_key = false;
            if let Some(s) = &spaces {
                chunks.push_str(s);
            }
            chunks.push_str(&self.prefix(length));
            self.forward(length);
            end = self.mark();
            spaces = self.scan_plain_spaces();
            match &spaces {
                None => break,
                Some(s) if s.is_empty() => break,
                _ => {}
            }
            if self.ch() == '#' || (self.flow_level == 0 && (self.column as isize) < indent) {
                break;
            }
        }
        Ok(Token {
            kind: Tok::Scalar {
                value: chunks,
                plain: true,
            },
            start,
            end,
        })
    }

    /// `None` is Python's bare `return` (a document separator follows);
    /// `Some("")` is an empty chunk list. Both stop the plain scalar.
    fn scan_plain_spaces(&mut self) -> Option<String> {
        let mut chunks = String::new();
        let mut length = 0;
        while self.peek(length) == ' ' {
            length += 1;
        }
        let whitespaces = self.prefix(length);
        self.forward(length);
        let ch = self.ch();
        if in_set(ch, BREAKS) {
            let line_break = self.scan_line_break();
            self.allow_simple_key = true;
            let prefix = self.prefix(3);
            if (prefix == "---" || prefix == "...") && in_set(self.peek(3), BLANKZ) {
                return None;
            }
            let mut breaks = String::new();
            while in_set(self.ch(), " \r\n\u{85}\u{2028}\u{2029}") {
                if self.ch() == ' ' {
                    self.forward(1);
                } else {
                    breaks.push_str(&self.scan_line_break());
                    let prefix = self.prefix(3);
                    if (prefix == "---" || prefix == "...") && in_set(self.peek(3), BLANKZ) {
                        return None;
                    }
                }
            }
            if line_break != "\n" {
                chunks.push_str(&line_break);
            } else if breaks.is_empty() {
                chunks.push(' ');
            }
            chunks.push_str(&breaks);
        } else if !whitespaces.is_empty() {
            chunks.push_str(&whitespaces);
        }
        Some(chunks)
    }

    fn scan_tag_handle(&mut self, name: &str, start: Mark) -> R<String> {
        let ch = self.ch();
        if ch != '!' {
            return Err(YamlError::marked(
                Some(&format!("while scanning a {name}")),
                Some(start),
                format!("expected '!', but found {}", py_repr_char(ch)),
                self.mark(),
            ));
        }
        let mut length = 1;
        let mut ch = self.peek(length);
        if ch != ' ' {
            while is_word(ch) {
                length += 1;
                ch = self.peek(length);
            }
            if ch != '!' {
                self.forward(length);
                return Err(YamlError::marked(
                    Some(&format!("while scanning a {name}")),
                    Some(start),
                    format!("expected '!', but found {}", py_repr_char(ch)),
                    self.mark(),
                ));
            }
            length += 1;
        }
        let value = self.prefix(length);
        self.forward(length);
        Ok(value)
    }

    fn scan_tag_uri(&mut self, name: &str, start: Mark) -> R<String> {
        let mut chunks = String::new();
        let mut length = 0;
        let mut ch = self.peek(length);
        while ch.is_ascii_alphanumeric() || in_set(ch, "-;/?:@&=+$,_.!~*'()[]%") {
            if ch == '%' {
                chunks.push_str(&self.prefix(length));
                self.forward(length);
                length = 0;
                chunks.push_str(&self.scan_uri_escapes(name, start)?);
            } else {
                length += 1;
            }
            ch = self.peek(length);
        }
        if length > 0 {
            chunks.push_str(&self.prefix(length));
            self.forward(length);
        }
        if chunks.is_empty() {
            return Err(YamlError::marked(
                Some(&format!("while parsing a {name}")),
                Some(start),
                format!("expected URI, but found {}", py_repr_char(ch)),
                self.mark(),
            ));
        }
        Ok(chunks)
    }

    fn scan_uri_escapes(&mut self, name: &str, start: Mark) -> R<String> {
        let mut codes = Vec::new();
        let mark = self.mark();
        while self.ch() == '%' {
            self.forward(1);
            for k in 0..2 {
                if !self.peek(k).is_ascii_hexdigit() {
                    return Err(YamlError::marked(
                        Some(&format!("while scanning a {name}")),
                        Some(start),
                        format!(
                            "expected URI escape sequence of 2 hexadecimal numbers, but found {}",
                            py_repr_char(self.peek(k))
                        ),
                        self.mark(),
                    ));
                }
            }
            codes.push(u8::from_str_radix(&self.prefix(2), 16).unwrap_or(0));
            self.forward(2);
        }
        String::from_utf8(codes).map_err(|e| {
            YamlError::marked(
                Some(&format!("while scanning a {name}")),
                Some(start),
                e.to_string(),
                mark,
            )
        })
    }

    fn scan_line_break(&mut self) -> String {
        let ch = self.ch();
        if in_set(ch, "\r\n\u{85}") {
            if self.prefix(2) == "\r\n" {
                self.forward(2);
            } else {
                self.forward(1);
            }
            return "\n".into();
        } else if ch == '\u{2028}' || ch == '\u{2029}' {
            self.forward(1);
            return ch.to_string();
        }
        String::new()
    }
}

fn escape_replacement(ch: char) -> Option<char> {
    Some(match ch {
        '0' => '\0',
        'a' => '\x07',
        'b' => '\x08',
        't' | '\t' => '\x09',
        'n' => '\x0A',
        'v' => '\x0B',
        'f' => '\x0C',
        'r' => '\x0D',
        'e' => '\x1B',
        ' ' => ' ',
        '"' => '"',
        '\\' => '\\',
        '/' => '/',
        'N' => '\u{85}',
        '_' => '\u{A0}',
        'L' => '\u{2028}',
        'P' => '\u{2029}',
        _ => return None,
    })
}

fn escape_code_len(ch: char) -> Option<usize> {
    match ch {
        'x' => Some(2),
        'u' => Some(4),
        'U' => Some(8),
        _ => None,
    }
}

/// Python's `%r` of a one-character string, close enough for messages.
fn py_repr_char(ch: char) -> String {
    match ch {
        '\0' => "'\\x00'".into(),
        '\'' => "\"'\"".into(),
        '\t' => "'\\t'".into(),
        '\n' => "'\\n'".into(),
        '\r' => "'\\r'".into(),
        c => format!("'{c}'"),
    }
}
