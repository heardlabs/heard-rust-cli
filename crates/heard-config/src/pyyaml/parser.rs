//! `yaml/parser.py`, ported state for state.

use super::scanner::{Directive, Mark, Scanner, Tok, Token};
use super::YamlError;

#[derive(Clone, Debug)]
pub(crate) enum Ev {
    StreamStart,
    StreamEnd,
    DocumentStart,
    DocumentEnd,
    Alias(String),
    Scalar {
        anchor: Option<String>,
        tag: Option<String>,
        /// `(plain-implicit, quoted-implicit)`.
        implicit: (bool, bool),
        value: String,
    },
    SequenceStart {
        anchor: Option<String>,
        tag: Option<String>,
    },
    SequenceEnd,
    MappingStart {
        anchor: Option<String>,
        tag: Option<String>,
    },
    MappingEnd,
}

#[derive(Clone, Debug)]
pub(crate) struct Event {
    pub kind: Ev,
    pub start: Mark,
}

#[derive(Clone, Copy, Debug)]
enum State {
    StreamStart,
    ImplicitDocumentStart,
    DocumentStart,
    DocumentEnd,
    DocumentContent,
    BlockNode,
    BlockSequenceFirstEntry,
    BlockSequenceEntry,
    IndentlessSequenceEntry,
    BlockMappingFirstKey,
    BlockMappingKey,
    BlockMappingValue,
    FlowSequenceFirstEntry,
    FlowSequenceEntry,
    FlowSequenceEntryMappingKey,
    FlowSequenceEntryMappingValue,
    FlowSequenceEntryMappingEnd,
    FlowMappingFirstKey,
    FlowMappingKey,
    FlowMappingValue,
    FlowMappingEmptyValue,
}

type R<T> = Result<T, YamlError>;

pub(crate) struct Parser {
    scanner: Scanner,
    current: Option<Event>,
    tag_handles: Vec<(String, String)>,
    states: Vec<State>,
    marks: Vec<Mark>,
    state: Option<State>,
}

fn default_tags() -> Vec<(String, String)> {
    vec![
        ("!".into(), "!".into()),
        ("!!".into(), "tag:yaml.org,2002:".into()),
    ]
}

impl Parser {
    pub(crate) fn new(scanner: Scanner) -> Self {
        Parser {
            scanner,
            current: None,
            tag_handles: Vec::new(),
            states: Vec::new(),
            marks: Vec::new(),
            state: Some(State::StreamStart),
        }
    }

    pub(crate) fn saw_surrogate(&self) -> bool {
        self.scanner.saw_surrogate
    }

    fn fill(&mut self) -> R<()> {
        if self.current.is_none() {
            if let Some(state) = self.state {
                self.current = Some(self.run(state)?);
            }
        }
        Ok(())
    }

    pub(crate) fn peek_event(&mut self) -> R<Option<&Event>> {
        self.fill()?;
        Ok(self.current.as_ref())
    }

    pub(crate) fn get_event(&mut self) -> R<Option<Event>> {
        self.fill()?;
        Ok(self.current.take())
    }

    fn check_token(&mut self, pred: fn(&Tok) -> bool) -> R<bool> {
        self.scanner.check(pred)
    }

    fn peek_token(&mut self) -> R<Token> {
        Ok(self
            .scanner
            .peek_token()?
            .cloned()
            .expect("the scanner always ends with a stream-end token"))
    }

    fn get_token(&mut self) -> R<Token> {
        Ok(self
            .scanner
            .get_token()?
            .expect("the scanner always ends with a stream-end token"))
    }

    fn pop_state(&mut self) -> State {
        self.states.pop().unwrap_or(State::DocumentEnd)
    }

    fn run(&mut self, state: State) -> R<Event> {
        match state {
            State::StreamStart => {
                let token = self.get_token()?;
                self.state = Some(State::ImplicitDocumentStart);
                Ok(Event {
                    kind: Ev::StreamStart,
                    start: token.start,
                })
            }
            State::ImplicitDocumentStart => {
                if !self.check_token(|t| {
                    matches!(t, Tok::Directive(..) | Tok::DocumentStart | Tok::StreamEnd)
                })? {
                    self.tag_handles = default_tags();
                    let token = self.peek_token()?;
                    self.states.push(State::DocumentEnd);
                    self.state = Some(State::BlockNode);
                    Ok(Event {
                        kind: Ev::DocumentStart,
                        start: token.start,
                    })
                } else {
                    self.parse_document_start()
                }
            }
            State::DocumentStart => self.parse_document_start(),
            State::DocumentEnd => {
                let token = self.peek_token()?;
                if self.check_token(|t| matches!(t, Tok::DocumentEnd))? {
                    self.get_token()?;
                }
                self.state = Some(State::DocumentStart);
                Ok(Event {
                    kind: Ev::DocumentEnd,
                    start: token.start,
                })
            }
            State::DocumentContent => {
                if self.check_token(|t| {
                    matches!(
                        t,
                        Tok::Directive(..) | Tok::DocumentStart | Tok::DocumentEnd | Tok::StreamEnd
                    )
                })? {
                    let mark = self.peek_token()?.start;
                    self.state = Some(self.pop_state());
                    Ok(empty_scalar(mark))
                } else {
                    self.parse_node(true, false)
                }
            }
            State::BlockNode => self.parse_node(true, false),
            State::BlockSequenceFirstEntry => {
                let token = self.get_token()?;
                self.marks.push(token.start);
                self.parse_block_sequence_entry()
            }
            State::BlockSequenceEntry => self.parse_block_sequence_entry(),
            State::IndentlessSequenceEntry => {
                if self.check_token(|t| matches!(t, Tok::BlockEntry))? {
                    let token = self.get_token()?;
                    if !self.check_token(|t| {
                        matches!(t, Tok::BlockEntry | Tok::Key | Tok::Value | Tok::BlockEnd)
                    })? {
                        self.states.push(State::IndentlessSequenceEntry);
                        return self.parse_node(true, false);
                    }
                    self.state = Some(State::IndentlessSequenceEntry);
                    return Ok(empty_scalar(token.end));
                }
                let token = self.peek_token()?;
                self.state = Some(self.pop_state());
                Ok(Event {
                    kind: Ev::SequenceEnd,
                    start: token.start,
                })
            }
            State::BlockMappingFirstKey => {
                let token = self.get_token()?;
                self.marks.push(token.start);
                self.parse_block_mapping_key()
            }
            State::BlockMappingKey => self.parse_block_mapping_key(),
            State::BlockMappingValue => {
                if self.check_token(|t| matches!(t, Tok::Value))? {
                    let token = self.get_token()?;
                    if !self.check_token(|t| matches!(t, Tok::Key | Tok::Value | Tok::BlockEnd))? {
                        self.states.push(State::BlockMappingKey);
                        return self.parse_node(true, true);
                    }
                    self.state = Some(State::BlockMappingKey);
                    return Ok(empty_scalar(token.end));
                }
                self.state = Some(State::BlockMappingKey);
                let token = self.peek_token()?;
                Ok(empty_scalar(token.start))
            }
            State::FlowSequenceFirstEntry => {
                let token = self.get_token()?;
                self.marks.push(token.start);
                self.parse_flow_sequence_entry(true)
            }
            State::FlowSequenceEntry => self.parse_flow_sequence_entry(false),
            State::FlowSequenceEntryMappingKey => {
                let token = self.get_token()?;
                if !self.check_token(|t| {
                    matches!(t, Tok::Value | Tok::FlowEntry | Tok::FlowSequenceEnd)
                })? {
                    self.states.push(State::FlowSequenceEntryMappingValue);
                    return self.parse_node(false, false);
                }
                self.state = Some(State::FlowSequenceEntryMappingValue);
                Ok(empty_scalar(token.end))
            }
            State::FlowSequenceEntryMappingValue => {
                if self.check_token(|t| matches!(t, Tok::Value))? {
                    let token = self.get_token()?;
                    if !self.check_token(|t| matches!(t, Tok::FlowEntry | Tok::FlowSequenceEnd))? {
                        self.states.push(State::FlowSequenceEntryMappingEnd);
                        return self.parse_node(false, false);
                    }
                    self.state = Some(State::FlowSequenceEntryMappingEnd);
                    return Ok(empty_scalar(token.end));
                }
                self.state = Some(State::FlowSequenceEntryMappingEnd);
                let token = self.peek_token()?;
                Ok(empty_scalar(token.start))
            }
            State::FlowSequenceEntryMappingEnd => {
                self.state = Some(State::FlowSequenceEntry);
                let token = self.peek_token()?;
                Ok(Event {
                    kind: Ev::MappingEnd,
                    start: token.start,
                })
            }
            State::FlowMappingFirstKey => {
                let token = self.get_token()?;
                self.marks.push(token.start);
                self.parse_flow_mapping_key(true)
            }
            State::FlowMappingKey => self.parse_flow_mapping_key(false),
            State::FlowMappingValue => {
                if self.check_token(|t| matches!(t, Tok::Value))? {
                    let token = self.get_token()?;
                    if !self.check_token(|t| matches!(t, Tok::FlowEntry | Tok::FlowMappingEnd))? {
                        self.states.push(State::FlowMappingKey);
                        return self.parse_node(false, false);
                    }
                    self.state = Some(State::FlowMappingKey);
                    return Ok(empty_scalar(token.end));
                }
                self.state = Some(State::FlowMappingKey);
                let token = self.peek_token()?;
                Ok(empty_scalar(token.start))
            }
            State::FlowMappingEmptyValue => {
                self.state = Some(State::FlowMappingKey);
                let token = self.peek_token()?;
                Ok(empty_scalar(token.start))
            }
        }
    }

    fn parse_document_start(&mut self) -> R<Event> {
        while self.check_token(|t| matches!(t, Tok::DocumentEnd))? {
            self.get_token()?;
        }
        if !self.check_token(|t| matches!(t, Tok::StreamEnd))? {
            let start = self.peek_token()?.start;
            self.process_directives()?;
            if !self.check_token(|t| matches!(t, Tok::DocumentStart))? {
                let token = self.peek_token()?;
                return Err(YamlError::marked(
                    None,
                    None,
                    format!(
                        "expected '<document start>', but found '{}'",
                        token.kind.id()
                    ),
                    token.start,
                ));
            }
            self.get_token()?;
            self.states.push(State::DocumentEnd);
            self.state = Some(State::DocumentContent);
            Ok(Event {
                kind: Ev::DocumentStart,
                start,
            })
        } else {
            let token = self.get_token()?;
            self.state = None;
            Ok(Event {
                kind: Ev::StreamEnd,
                start: token.start,
            })
        }
    }

    fn process_directives(&mut self) -> R<()> {
        let mut yaml_version: Option<(u64, u64)> = None;
        self.tag_handles = Vec::new();
        while self.check_token(|t| matches!(t, Tok::Directive(..)))? {
            let token = self.get_token()?;
            let Tok::Directive(name, value) = token.kind else {
                unreachable!("checked above")
            };
            if name == "YAML" {
                if yaml_version.is_some() {
                    return Err(YamlError::marked(
                        None,
                        None,
                        "found duplicate YAML directive".into(),
                        token.start,
                    ));
                }
                let Directive::Yaml(major, minor) = value else {
                    unreachable!("a YAML directive always carries a version")
                };
                if major != 1 {
                    return Err(YamlError::marked(
                        None,
                        None,
                        "found incompatible YAML document (version 1.* is required)".into(),
                        token.start,
                    ));
                }
                yaml_version = Some((major, minor));
            } else if name == "TAG" {
                let Directive::Tag(handle, prefix) = value else {
                    unreachable!("a TAG directive always carries a handle and prefix")
                };
                if self.tag_handles.iter().any(|(h, _)| *h == handle) {
                    return Err(YamlError::marked(
                        None,
                        None,
                        format!("duplicate tag handle '{handle}'"),
                        token.start,
                    ));
                }
                self.tag_handles.push((handle, prefix));
            }
        }
        for (k, v) in default_tags() {
            if !self.tag_handles.iter().any(|(h, _)| *h == k) {
                self.tag_handles.push((k, v));
            }
        }
        Ok(())
    }

    fn parse_node(&mut self, block: bool, indentless_sequence: bool) -> R<Event> {
        if self.check_token(|t| matches!(t, Tok::Alias(_)))? {
            let token = self.get_token()?;
            let Tok::Alias(name) = token.kind else {
                unreachable!("checked above")
            };
            self.state = Some(self.pop_state());
            return Ok(Event {
                kind: Ev::Alias(name),
                start: token.start,
            });
        }
        let mut anchor = None;
        let mut tag: Option<(Option<String>, String)> = None;
        let mut start: Option<Mark> = None;
        let mut tag_mark: Option<Mark> = None;
        if self.check_token(|t| matches!(t, Tok::Anchor(_)))? {
            let token = self.get_token()?;
            start = Some(token.start);
            if let Tok::Anchor(a) = token.kind {
                anchor = Some(a);
            }
            if self.check_token(|t| matches!(t, Tok::Tag(..)))? {
                let token = self.get_token()?;
                tag_mark = Some(token.start);
                if let Tok::Tag(h, s) = token.kind {
                    tag = Some((h, s));
                }
            }
        } else if self.check_token(|t| matches!(t, Tok::Tag(..)))? {
            let token = self.get_token()?;
            start = Some(token.start);
            tag_mark = Some(token.start);
            if let Tok::Tag(h, s) = token.kind {
                tag = Some((h, s));
            }
            if self.check_token(|t| matches!(t, Tok::Anchor(_)))? {
                let token = self.get_token()?;
                if let Tok::Anchor(a) = token.kind {
                    anchor = Some(a);
                }
            }
        }
        let tag: Option<String> = match tag {
            None => None,
            Some((Some(handle), suffix)) => {
                let Some((_, prefix)) = self.tag_handles.iter().find(|(h, _)| *h == handle) else {
                    return Err(YamlError::marked(
                        Some("while parsing a node"),
                        start,
                        format!("found undefined tag handle '{handle}'"),
                        tag_mark.unwrap_or_default(),
                    ));
                };
                Some(format!("{prefix}{suffix}"))
            }
            Some((None, suffix)) => Some(suffix),
        };
        let start = match start {
            Some(s) => s,
            None => self.peek_token()?.start,
        };
        let implicit = tag.is_none() || tag.as_deref() == Some("!");
        if indentless_sequence && self.check_token(|t| matches!(t, Tok::BlockEntry))? {
            self.state = Some(State::IndentlessSequenceEntry);
            return Ok(Event {
                kind: Ev::SequenceStart { anchor, tag },
                start,
            });
        }
        if self.check_token(|t| matches!(t, Tok::Scalar { .. }))? {
            let token = self.get_token()?;
            let Tok::Scalar { value, plain, .. } = token.kind else {
                unreachable!("checked above")
            };
            let implicit = if (plain && tag.is_none()) || tag.as_deref() == Some("!") {
                (true, false)
            } else if tag.is_none() {
                (false, true)
            } else {
                (false, false)
            };
            self.state = Some(self.pop_state());
            return Ok(Event {
                kind: Ev::Scalar {
                    anchor,
                    tag,
                    implicit,
                    value,
                },
                start,
            });
        }
        if self.check_token(|t| matches!(t, Tok::FlowSequenceStart))? {
            self.state = Some(State::FlowSequenceFirstEntry);
            return Ok(Event {
                kind: Ev::SequenceStart { anchor, tag },
                start,
            });
        }
        if self.check_token(|t| matches!(t, Tok::FlowMappingStart))? {
            self.state = Some(State::FlowMappingFirstKey);
            return Ok(Event {
                kind: Ev::MappingStart { anchor, tag },
                start,
            });
        }
        if block && self.check_token(|t| matches!(t, Tok::BlockSequenceStart))? {
            self.state = Some(State::BlockSequenceFirstEntry);
            return Ok(Event {
                kind: Ev::SequenceStart { anchor, tag },
                start,
            });
        }
        if block && self.check_token(|t| matches!(t, Tok::BlockMappingStart))? {
            self.state = Some(State::BlockMappingFirstKey);
            return Ok(Event {
                kind: Ev::MappingStart { anchor, tag },
                start,
            });
        }
        if anchor.is_some() || tag.is_some() {
            self.state = Some(self.pop_state());
            return Ok(Event {
                kind: Ev::Scalar {
                    anchor,
                    tag,
                    implicit: (implicit, false),
                    value: String::new(),
                },
                start,
            });
        }
        let node = if block { "block" } else { "flow" };
        let token = self.peek_token()?;
        Err(YamlError::marked(
            Some(&format!("while parsing a {node} node")),
            Some(start),
            format!("expected the node content, but found '{}'", token.kind.id()),
            token.start,
        ))
    }

    fn parse_block_sequence_entry(&mut self) -> R<Event> {
        if self.check_token(|t| matches!(t, Tok::BlockEntry))? {
            let token = self.get_token()?;
            if !self.check_token(|t| matches!(t, Tok::BlockEntry | Tok::BlockEnd))? {
                self.states.push(State::BlockSequenceEntry);
                return self.parse_node(true, false);
            }
            self.state = Some(State::BlockSequenceEntry);
            return Ok(empty_scalar(token.end));
        }
        if !self.check_token(|t| matches!(t, Tok::BlockEnd))? {
            let token = self.peek_token()?;
            return Err(YamlError::marked(
                Some("while parsing a block collection"),
                self.marks.last().copied(),
                format!("expected <block end>, but found '{}'", token.kind.id()),
                token.start,
            ));
        }
        let token = self.get_token()?;
        self.state = Some(self.pop_state());
        self.marks.pop();
        Ok(Event {
            kind: Ev::SequenceEnd,
            start: token.start,
        })
    }

    fn parse_block_mapping_key(&mut self) -> R<Event> {
        if self.check_token(|t| matches!(t, Tok::Key))? {
            let token = self.get_token()?;
            if !self.check_token(|t| matches!(t, Tok::Key | Tok::Value | Tok::BlockEnd))? {
                self.states.push(State::BlockMappingValue);
                return self.parse_node(true, true);
            }
            self.state = Some(State::BlockMappingValue);
            return Ok(empty_scalar(token.end));
        }
        if !self.check_token(|t| matches!(t, Tok::BlockEnd))? {
            let token = self.peek_token()?;
            return Err(YamlError::marked(
                Some("while parsing a block mapping"),
                self.marks.last().copied(),
                format!("expected <block end>, but found '{}'", token.kind.id()),
                token.start,
            ));
        }
        let token = self.get_token()?;
        self.state = Some(self.pop_state());
        self.marks.pop();
        Ok(Event {
            kind: Ev::MappingEnd,
            start: token.start,
        })
    }

    fn parse_flow_sequence_entry(&mut self, first: bool) -> R<Event> {
        if !self.check_token(|t| matches!(t, Tok::FlowSequenceEnd))? {
            if !first {
                if self.check_token(|t| matches!(t, Tok::FlowEntry))? {
                    self.get_token()?;
                } else {
                    let token = self.peek_token()?;
                    return Err(YamlError::marked(
                        Some("while parsing a flow sequence"),
                        self.marks.last().copied(),
                        format!("expected ',' or ']', but got '{}'", token.kind.id()),
                        token.start,
                    ));
                }
            }
            if self.check_token(|t| matches!(t, Tok::Key))? {
                let token = self.peek_token()?;
                self.state = Some(State::FlowSequenceEntryMappingKey);
                return Ok(Event {
                    kind: Ev::MappingStart {
                        anchor: None,
                        tag: None,
                    },
                    start: token.start,
                });
            } else if !self.check_token(|t| matches!(t, Tok::FlowSequenceEnd))? {
                self.states.push(State::FlowSequenceEntry);
                return self.parse_node(false, false);
            }
        }
        let token = self.get_token()?;
        self.state = Some(self.pop_state());
        self.marks.pop();
        Ok(Event {
            kind: Ev::SequenceEnd,
            start: token.start,
        })
    }

    fn parse_flow_mapping_key(&mut self, first: bool) -> R<Event> {
        if !self.check_token(|t| matches!(t, Tok::FlowMappingEnd))? {
            if !first {
                if self.check_token(|t| matches!(t, Tok::FlowEntry))? {
                    self.get_token()?;
                } else {
                    let token = self.peek_token()?;
                    return Err(YamlError::marked(
                        Some("while parsing a flow mapping"),
                        self.marks.last().copied(),
                        format!("expected ',' or '}}', but got '{}'", token.kind.id()),
                        token.start,
                    ));
                }
            }
            if self.check_token(|t| matches!(t, Tok::Key))? {
                let token = self.get_token()?;
                if !self.check_token(|t| {
                    matches!(t, Tok::Value | Tok::FlowEntry | Tok::FlowMappingEnd)
                })? {
                    self.states.push(State::FlowMappingValue);
                    return self.parse_node(false, false);
                }
                self.state = Some(State::FlowMappingValue);
                return Ok(empty_scalar(token.end));
            } else if !self.check_token(|t| matches!(t, Tok::FlowMappingEnd))? {
                self.states.push(State::FlowMappingEmptyValue);
                return self.parse_node(false, false);
            }
        }
        let token = self.get_token()?;
        self.state = Some(self.pop_state());
        self.marks.pop();
        Ok(Event {
            kind: Ev::MappingEnd,
            start: token.start,
        })
    }
}

fn empty_scalar(mark: Mark) -> Event {
    Event {
        kind: Ev::Scalar {
            anchor: None,
            tag: None,
            implicit: (true, false),
            value: String::new(),
        },
        start: mark,
    }
}
