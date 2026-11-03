//! The YAML output (`YAMLDumper` in `awscli/formatter.py`).
//!
//! `YAML(typ='safe')` with `ruamel.yaml.clib` installed (as in the
//! reference CLI) represents values with `ruamel.yaml.representer.
//! SafeRepresenter`, resolves implicit tags with the YAML 1.2
//! `ruamel.yaml.resolver.Resolver`, serializes with `_ruamel_yaml.pyx`
//! `CEmitter.serialize`, and emits with the libyaml copy in
//! `ruamel.yaml.clib` (`emitter.c`). This module ports those layers for
//! the values the CLI produces.

use std::sync::LazyLock;

use base64::Engine;
use regex::Regex;

use crate::pyfmt::{float_repr, json_dumps_line};
use crate::value::Value;

/// `YAMLDumper.dump`: JSON for top-level scalars, YAML otherwise.
pub fn dump(value: &Value, out: &mut String) {
    match value {
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Str(_) => {
            out.push_str(&json_dumps_line(value));
            out.push('\n');
        }
        _ => out.push_str(&yaml_dump(value)),
    }
}

const TAG_STR: &str = "tag:yaml.org,2002:str";
const TAG_INT: &str = "tag:yaml.org,2002:int";
const TAG_FLOAT: &str = "tag:yaml.org,2002:float";
const TAG_BOOL: &str = "tag:yaml.org,2002:bool";
const TAG_NULL: &str = "tag:yaml.org,2002:null";
const TAG_BINARY: &str = "tag:yaml.org,2002:binary";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScalarStyle {
    Plain,
    SingleQuoted,
    DoubleQuoted,
    Literal,
    Folded,
}

#[derive(Debug, Clone)]
enum Event {
    StreamStart,
    StreamEnd,
    DocumentStart,
    DocumentEnd,
    Scalar {
        tag: &'static str,
        value: String,
        plain_implicit: bool,
        quoted_implicit: bool,
        style: Option<ScalarStyle>,
    },
    SequenceStart {
        flow: bool,
    },
    SequenceEnd,
    MappingStart {
        flow: bool,
    },
    MappingEnd,
}

/// `YAML.dump` of one document.
pub fn yaml_dump(value: &Value) -> String {
    let mut events = vec![Event::StreamStart, Event::DocumentStart];
    represent(value, &mut events);
    events.push(Event::DocumentEnd);
    events.push(Event::StreamEnd);
    let mut emitter = Emitter::new();
    emitter.emit_all(&events);
    String::from_utf8(emitter.buffer).expect("emitter output is UTF-8")
}

/// `SafeRepresenter.represent_*` plus `CEmitter._serialize_node`.
fn represent(value: &Value, events: &mut Vec<Event>) {
    match value {
        Value::Null => scalar(TAG_NULL, "null".to_string(), None, events),
        Value::Bool(b) => scalar(
            TAG_BOOL,
            if *b { "true" } else { "false" }.to_string(),
            None,
            events,
        ),
        Value::Int(i) => scalar(TAG_INT, i.to_string(), None, events),
        Value::Float(f) => {
            let v = if f.is_nan() {
                ".nan".to_string()
            } else if f.is_infinite() {
                if *f > 0.0 { ".inf" } else { "-.inf" }.to_string()
            } else {
                float_repr(*f).to_lowercase()
            };
            scalar(TAG_FLOAT, v, None, events)
        }
        Value::Str(s) => scalar(TAG_STR, s.clone(), None, events),
        Value::Bytes(b) => scalar(
            TAG_BINARY,
            encodebytes(b),
            Some(ScalarStyle::Literal),
            events,
        ),
        Value::List(l) => {
            events.push(Event::SequenceStart { flow: false });
            for item in l {
                represent(item, events);
            }
            events.push(Event::SequenceEnd);
        }
        Value::Map(m) => {
            events.push(Event::MappingStart { flow: false });
            let mut entries: Vec<(&String, &Value)> = m.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (k, v) in entries {
                scalar(TAG_STR, k.clone(), None, events);
                represent(v, events);
            }
            events.push(Event::MappingEnd);
        }
    }
}

fn scalar(tag: &'static str, value: String, style: Option<ScalarStyle>, events: &mut Vec<Event>) {
    let plain_implicit = resolve_plain(&value) == tag;
    let quoted_implicit = tag == TAG_STR;
    events.push(Event::Scalar {
        tag,
        value,
        plain_implicit,
        quoted_implicit,
        style,
    });
}

/// Python `base64.encodebytes`: lines of at most 76 characters, each
/// ending with a newline.
fn encodebytes(b: &[u8]) -> String {
    let mut out = String::new();
    for chunk in b.chunks(57) {
        out.push_str(&base64::engine::general_purpose::STANDARD.encode(chunk));
        out.push('\n');
    }
    out
}

struct Resolver {
    first: &'static str,
    tag: &'static str,
    regex: Regex,
}

/// The YAML 1.2 implicit resolvers of `ruamel/yaml/resolver.py`, in
/// registration order. Python's `$` also matches before a final newline.
static RESOLVERS: LazyLock<Vec<Resolver>> = LazyLock::new(|| {
    let r = |first: &'static str, tag: &'static str, re: &str| Resolver {
        first,
        tag,
        regex: Regex::new(&format!(r"\A(?:{re})\n?\z")).expect("resolver regex"),
    };
    vec![
        r("tTfF", TAG_BOOL, r"true|True|TRUE|false|False|FALSE"),
        r(
            "-+0123456789.",
            TAG_FLOAT,
            r"[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+]?[0-9]+)?|[-+]?(?:[0-9][0-9_]*)(?:[eE][-+]?[0-9]+)|[-+]?\.[0-9_]+(?:[eE][-+][0-9]+)?|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN)",
        ),
        r(
            "-+0123456789",
            TAG_INT,
            r"[-+]?0b[0-1_]+|[-+]?0o?[0-7_]+|[-+]?[0-9_]+|[-+]?0x[0-9a-fA-F_]+",
        ),
        r("<", "tag:yaml.org,2002:merge", r"<<"),
        r("~nN", TAG_NULL, r"~|null|Null|NULL|"),
        r(
            "0123456789",
            "tag:yaml.org,2002:timestamp",
            r"[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]|[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?(?:[Tt]|[ \t]+)[0-9][0-9]?:[0-9][0-9]:[0-9][0-9](?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?",
        ),
        r("=", "tag:yaml.org,2002:value", r"="),
        r("!&*", "tag:yaml.org,2002:yaml", r"!|&|\*"),
    ]
});

/// `BaseResolver.resolve(ScalarNode, value, (True, False))`.
fn resolve_plain(value: &str) -> &'static str {
    let first = value.chars().next();
    for r in RESOLVERS.iter() {
        let applies = match first {
            None => r.tag == TAG_NULL,
            Some(c) => r.first.contains(c),
        };
        if applies && r.regex.is_match(value) {
            return r.tag;
        }
    }
    TAG_STR
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    StreamStart,
    FirstDocumentStart,
    DocumentStart,
    DocumentContent,
    DocumentEnd,
    FlowSequenceFirstItem,
    FlowSequenceItem,
    FlowMappingFirstKey,
    FlowMappingKey,
    FlowMappingSimpleValue,
    FlowMappingValue,
    BlockSequenceFirstItem,
    BlockSequenceItem,
    BlockMappingFirstKey,
    BlockMappingKey,
    BlockMappingSimpleValue,
    BlockMappingValue,
    End,
}

#[derive(Default)]
struct ScalarAnalysis {
    value: Vec<u8>,
    multiline: bool,
    flow_plain_allowed: bool,
    block_plain_allowed: bool,
    single_quoted_allowed: bool,
    block_allowed: bool,
    style: Option<ScalarStyle>,
}

/// The libyaml emitter of `ruamel.yaml.clib` (`emitter.c`), without
/// anchors, directives, and canonical mode.
struct Emitter {
    buffer: Vec<u8>,
    best_indent: i32,
    best_width: i32,
    unicode: bool,
    state: State,
    states: Vec<State>,
    indent: i32,
    indents: Vec<i32>,
    flow_level: i32,
    root_context: bool,
    sequence_context: bool,
    mapping_context: bool,
    simple_key_context: bool,
    column: i32,
    whitespace: bool,
    indention: bool,
    open_ended: bool,
    tag_handle: Option<&'static str>,
    tag_suffix: Option<String>,
    scalar_data: ScalarAnalysis,
}

/// Byte at `i`, or NUL past the end (libyaml strings are NUL-terminated).
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

fn width_at(s: &[u8], i: usize) -> usize {
    let o = at(s, i);
    if o & 0x80 == 0 {
        1
    } else if o & 0xE0 == 0xC0 {
        2
    } else if o & 0xF0 == 0xE0 {
        3
    } else if o & 0xF8 == 0xF0 {
        4
    } else {
        0
    }
}

fn is_space_at(s: &[u8], i: usize) -> bool {
    at(s, i) == b' '
}

fn is_blank_at(s: &[u8], i: usize) -> bool {
    at(s, i) == b' ' || at(s, i) == b'\t'
}

fn is_break_at(s: &[u8], i: usize) -> bool {
    let c = at(s, i);
    c == b'\r'
        || c == b'\n'
        || (c == 0xC2 && at(s, i + 1) == 0x85)
        || (c == 0xE2 && at(s, i + 1) == 0x80 && at(s, i + 2) == 0xA8)
        || (c == 0xE2 && at(s, i + 1) == 0x80 && at(s, i + 2) == 0xA9)
}

fn is_blankz_at(s: &[u8], i: usize) -> bool {
    is_blank_at(s, i) || is_break_at(s, i) || at(s, i) == 0
}

fn is_printable_at(s: &[u8], i: usize) -> bool {
    let c = at(s, i);
    let c1 = at(s, i + 1);
    let c2 = at(s, i + 2);
    c == 0x0A
        || (0x20..=0x7E).contains(&c)
        || (c == 0xC2 && c1 >= 0xA0)
        || (c > 0xC2 && c < 0xED)
        || (c == 0xED && c1 < 0xA0)
        || c == 0xEE
        || (c == 0xEF && !(c1 == 0xBB && c2 == 0xBF) && !(c1 == 0xBF && (c2 == 0xBE || c2 == 0xBF)))
}

fn is_bom_at(s: &[u8], i: usize) -> bool {
    at(s, i) == 0xEF && at(s, i + 1) == 0xBB && at(s, i + 2) == 0xBF
}

fn is_alpha_at(s: &[u8], i: usize) -> bool {
    let c = at(s, i);
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

impl Emitter {
    fn new() -> Emitter {
        Emitter {
            buffer: Vec::new(),
            best_indent: 2,
            best_width: 80,
            unicode: true,
            state: State::StreamStart,
            states: Vec::new(),
            indent: -1,
            indents: Vec::new(),
            flow_level: 0,
            root_context: false,
            sequence_context: false,
            mapping_context: false,
            simple_key_context: false,
            column: 0,
            whitespace: true,
            indention: true,
            open_ended: false,
            tag_handle: None,
            tag_suffix: None,
            scalar_data: ScalarAnalysis::default(),
        }
    }

    fn emit_all(&mut self, events: &[Event]) {
        for i in 0..events.len() {
            self.analyze_event(&events[i]);
            self.state_machine(events, i);
        }
    }

    // Output primitives (the PUT / PUT_BREAK / WRITE / WRITE_BREAK macros).

    fn put(&mut self, c: u8) {
        self.buffer.push(c);
        self.column += 1;
    }

    fn put_break(&mut self) {
        self.buffer.push(b'\n');
        self.column = 0;
    }

    fn write(&mut self, s: &[u8], p: &mut usize) {
        let w = width_at(s, *p).max(1);
        self.buffer.extend_from_slice(&s[*p..(*p + w).min(s.len())]);
        *p += w;
        self.column += 1;
    }

    fn write_break(&mut self, s: &[u8], p: &mut usize) {
        if at(s, *p) == b'\n' {
            self.put_break();
            *p += 1;
        } else {
            let w = width_at(s, *p).max(1);
            self.buffer.extend_from_slice(&s[*p..(*p + w).min(s.len())]);
            *p += w;
            self.column = 0;
        }
    }

    fn increase_indent(&mut self, flow: bool, indentless: bool) {
        self.indents.push(self.indent);
        if self.indent < 0 {
            self.indent = if flow { self.best_indent } else { 0 };
        } else if !indentless {
            self.indent += self.best_indent;
        }
    }

    fn pop_indent(&mut self) {
        self.indent = self.indents.pop().expect("indent stack");
    }

    fn pop_state(&mut self) {
        self.state = self.states.pop().expect("state stack");
    }

    /// `yaml_emitter_analyze_event`.
    fn analyze_event(&mut self, event: &Event) {
        self.tag_handle = None;
        self.tag_suffix = None;
        self.scalar_data.value.clear();
        if let Event::Scalar {
            tag,
            value,
            plain_implicit,
            quoted_implicit,
            ..
        } = event
        {
            if !plain_implicit && !quoted_implicit {
                self.analyze_tag(tag);
            }
            self.analyze_scalar(value.as_bytes());
        }
    }

    /// `yaml_emitter_analyze_tag` with the default tag directives.
    fn analyze_tag(&mut self, tag: &str) {
        for (handle, prefix) in [("!", "!"), ("!!", "tag:yaml.org,2002:")] {
            if prefix.len() < tag.len() && tag.starts_with(prefix) {
                self.tag_handle = Some(handle);
                self.tag_suffix = Some(tag[prefix.len()..].to_string());
                return;
            }
        }
        self.tag_suffix = Some(tag.to_string());
    }

    /// `yaml_emitter_analyze_scalar`.
    fn analyze_scalar(&mut self, s: &[u8]) {
        let d = &mut self.scalar_data;
        d.value = s.to_vec();
        if s.is_empty() {
            d.multiline = false;
            d.flow_plain_allowed = false;
            d.block_plain_allowed = true;
            d.single_quoted_allowed = true;
            d.block_allowed = false;
            return;
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
        let mut previous_space = false;
        let mut previous_break = false;

        if (at(s, 0) == b'-' && at(s, 1) == b'-' && at(s, 2) == b'-')
            || (at(s, 0) == b'.' && at(s, 1) == b'.' && at(s, 2) == b'.')
        {
            block_indicators = true;
            flow_indicators = true;
        }
        let mut preceded_by_whitespace = true;
        let mut followed_by_whitespace = is_blankz_at(s, width_at(s, 0));
        let mut p = 0;
        while p < s.len() {
            let c = s[p];
            if p == 0 {
                if b"#,[]{}&*!|>'\"%@`".contains(&c) {
                    flow_indicators = true;
                    block_indicators = true;
                }
                if c == b'?' || c == b':' {
                    flow_indicators = true;
                    if followed_by_whitespace {
                        block_indicators = true;
                    }
                }
                if c == b'-' && followed_by_whitespace {
                    flow_indicators = true;
                    block_indicators = true;
                }
            } else {
                if b",?[]{}".contains(&c) {
                    flow_indicators = true;
                }
                if c == b':' {
                    flow_indicators = true;
                    if followed_by_whitespace {
                        block_indicators = true;
                    }
                }
                if c == b'#' && preceded_by_whitespace {
                    flow_indicators = true;
                    block_indicators = true;
                }
            }
            if !is_printable_at(s, p) || (c > 0x7F && !self.unicode) {
                special_characters = true;
            }
            if is_break_at(s, p) {
                line_breaks = true;
            }
            let w = width_at(s, p).max(1);
            if is_space_at(s, p) {
                if p == 0 {
                    leading_space = true;
                }
                if p + w == s.len() {
                    trailing_space = true;
                }
                if previous_break {
                    break_space = true;
                }
                previous_space = true;
                previous_break = false;
            } else if is_break_at(s, p) {
                if p == 0 {
                    leading_break = true;
                }
                if p + w == s.len() {
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
            preceded_by_whitespace = is_blankz_at(s, p);
            p += w;
            if p < s.len() {
                followed_by_whitespace = is_blankz_at(s, p + width_at(s, p));
            }
        }

        d.multiline = line_breaks;
        d.flow_plain_allowed = true;
        d.block_plain_allowed = true;
        d.single_quoted_allowed = true;
        d.block_allowed = true;
        if leading_space || leading_break || trailing_space || trailing_break {
            d.flow_plain_allowed = false;
            d.block_plain_allowed = false;
        }
        if trailing_space {
            d.block_allowed = false;
        }
        if break_space {
            d.flow_plain_allowed = false;
            d.block_plain_allowed = false;
            d.single_quoted_allowed = false;
        }
        if space_break || special_characters {
            d.flow_plain_allowed = false;
            d.block_plain_allowed = false;
            d.single_quoted_allowed = false;
            d.block_allowed = false;
        }
        if line_breaks {
            d.flow_plain_allowed = false;
            d.block_plain_allowed = false;
        }
        if flow_indicators {
            d.flow_plain_allowed = false;
        }
        if block_indicators {
            d.block_plain_allowed = false;
        }
    }

    /// `yaml_emitter_state_machine`.
    fn state_machine(&mut self, events: &[Event], i: usize) {
        let event = &events[i];
        match self.state {
            State::StreamStart => {
                if matches!(event, Event::StreamStart) {
                    self.indent = -1;
                    self.column = 0;
                    self.whitespace = true;
                    self.indention = true;
                    self.state = State::FirstDocumentStart;
                }
            }
            State::FirstDocumentStart | State::DocumentStart => match event {
                Event::DocumentStart => {
                    // Implicit document start: nothing is written.
                    self.state = State::DocumentContent;
                }
                Event::StreamEnd => {
                    if self.open_ended {
                        self.write_indicator(b"...", true, false, false);
                        self.write_indent();
                    }
                    self.state = State::End;
                }
                _ => {}
            },
            State::DocumentContent => {
                self.states.push(State::DocumentEnd);
                self.emit_node(events, i, true, false, false, false);
            }
            State::DocumentEnd => {
                if matches!(event, Event::DocumentEnd) {
                    self.write_indent();
                    self.state = State::DocumentStart;
                }
            }
            State::FlowSequenceFirstItem => self.emit_flow_sequence_item(events, i, true),
            State::FlowSequenceItem => self.emit_flow_sequence_item(events, i, false),
            State::FlowMappingFirstKey => self.emit_flow_mapping_key(events, i, true),
            State::FlowMappingKey => self.emit_flow_mapping_key(events, i, false),
            State::FlowMappingSimpleValue => self.emit_flow_mapping_value(events, i, true),
            State::FlowMappingValue => self.emit_flow_mapping_value(events, i, false),
            State::BlockSequenceFirstItem => self.emit_block_sequence_item(events, i, true),
            State::BlockSequenceItem => self.emit_block_sequence_item(events, i, false),
            State::BlockMappingFirstKey => self.emit_block_mapping_key(events, i, true),
            State::BlockMappingKey => self.emit_block_mapping_key(events, i, false),
            State::BlockMappingSimpleValue => self.emit_block_mapping_value(events, i, true),
            State::BlockMappingValue => self.emit_block_mapping_value(events, i, false),
            State::End => {}
        }
    }

    fn emit_flow_sequence_item(&mut self, events: &[Event], i: usize, first: bool) {
        if first {
            self.write_indicator(b"[", true, true, false);
            self.increase_indent(true, false);
            self.flow_level += 1;
        }
        if matches!(events[i], Event::SequenceEnd) {
            self.flow_level -= 1;
            self.pop_indent();
            self.write_indicator(b"]", false, false, false);
            self.pop_state();
            return;
        }
        if !first {
            self.write_indicator(b",", false, false, false);
        }
        if self.column > self.best_width {
            self.write_indent();
        }
        self.states.push(State::FlowSequenceItem);
        self.emit_node(events, i, false, true, false, false);
    }

    fn emit_flow_mapping_key(&mut self, events: &[Event], i: usize, first: bool) {
        if first {
            self.write_indicator(b"{", true, true, false);
            self.increase_indent(true, false);
            self.flow_level += 1;
        }
        if matches!(events[i], Event::MappingEnd) {
            self.flow_level -= 1;
            self.pop_indent();
            self.write_indicator(b"}", false, false, false);
            self.pop_state();
            return;
        }
        if !first {
            self.write_indicator(b",", false, false, false);
        }
        if self.column > self.best_width {
            self.write_indent();
        }
        if self.check_simple_key(events, i) {
            self.states.push(State::FlowMappingSimpleValue);
            self.emit_node(events, i, false, false, true, true);
        } else {
            self.write_indicator(b"?", true, false, false);
            self.states.push(State::FlowMappingValue);
            self.emit_node(events, i, false, false, true, false);
        }
    }

    fn emit_flow_mapping_value(&mut self, events: &[Event], i: usize, simple: bool) {
        if simple {
            self.write_indicator(b":", false, false, false);
        } else {
            if self.column > self.best_width {
                self.write_indent();
            }
            self.write_indicator(b":", true, false, false);
        }
        self.states.push(State::FlowMappingKey);
        self.emit_node(events, i, false, false, true, false);
    }

    fn emit_block_sequence_item(&mut self, events: &[Event], i: usize, first: bool) {
        if first {
            let indentless = self.mapping_context && !self.indention;
            self.increase_indent(false, indentless);
        }
        if matches!(events[i], Event::SequenceEnd) {
            self.pop_indent();
            self.pop_state();
            return;
        }
        self.write_indent();
        self.write_indicator(b"-", true, false, true);
        self.states.push(State::BlockSequenceItem);
        self.emit_node(events, i, false, true, false, false);
    }

    fn emit_block_mapping_key(&mut self, events: &[Event], i: usize, first: bool) {
        if first {
            self.increase_indent(false, false);
        }
        if matches!(events[i], Event::MappingEnd) {
            self.pop_indent();
            self.pop_state();
            return;
        }
        self.write_indent();
        if self.check_simple_key(events, i) {
            self.states.push(State::BlockMappingSimpleValue);
            self.emit_node(events, i, false, false, true, true);
        } else {
            self.write_indicator(b"?", true, false, true);
            self.states.push(State::BlockMappingValue);
            self.emit_node(events, i, false, false, true, false);
        }
    }

    fn emit_block_mapping_value(&mut self, events: &[Event], i: usize, simple: bool) {
        if simple {
            self.write_indicator(b":", false, false, false);
        } else {
            self.write_indent();
            self.write_indicator(b":", true, false, true);
        }
        self.states.push(State::BlockMappingKey);
        self.emit_node(events, i, false, false, true, false);
    }

    fn emit_node(
        &mut self,
        events: &[Event],
        i: usize,
        root: bool,
        sequence: bool,
        mapping: bool,
        simple_key: bool,
    ) {
        self.root_context = root;
        self.sequence_context = sequence;
        self.mapping_context = mapping;
        self.simple_key_context = simple_key;
        match &events[i] {
            Event::Scalar { .. } => self.emit_scalar(&events[i]),
            Event::SequenceStart { flow } => {
                self.process_tag();
                if self.flow_level > 0 || *flow || check_empty(events, i) {
                    self.state = State::FlowSequenceFirstItem;
                } else {
                    self.state = State::BlockSequenceFirstItem;
                }
            }
            Event::MappingStart { flow } => {
                self.process_tag();
                if self.flow_level > 0 || *flow || check_empty(events, i) {
                    self.state = State::FlowMappingFirstKey;
                } else {
                    self.state = State::BlockMappingFirstKey;
                }
            }
            _ => {}
        }
    }

    /// `yaml_emitter_check_simple_key`.
    fn check_simple_key(&self, events: &[Event], i: usize) -> bool {
        let tag_len =
            self.tag_handle.map_or(0, str::len) + self.tag_suffix.as_ref().map_or(0, String::len);
        let length = match &events[i] {
            Event::Scalar { .. } => {
                if self.scalar_data.multiline {
                    return false;
                }
                tag_len + self.scalar_data.value.len()
            }
            Event::SequenceStart { .. } | Event::MappingStart { .. } => {
                if !check_empty(events, i) {
                    return false;
                }
                tag_len
            }
            _ => return false,
        };
        length <= 128
    }

    fn emit_scalar(&mut self, event: &Event) {
        self.select_scalar_style(event);
        self.process_tag();
        self.increase_indent(true, false);
        self.process_scalar();
        self.pop_indent();
        self.pop_state();
    }

    /// `yaml_emitter_select_scalar_style`.
    fn select_scalar_style(&mut self, event: &Event) {
        let Event::Scalar {
            plain_implicit,
            quoted_implicit,
            style,
            ..
        } = event
        else {
            return;
        };
        let no_tag = self.tag_handle.is_none() && self.tag_suffix.is_none();
        let d = &self.scalar_data;
        let mut style = style.unwrap_or(ScalarStyle::Plain);
        if self.simple_key_context && d.multiline {
            style = ScalarStyle::DoubleQuoted;
        }
        if style == ScalarStyle::Plain {
            if (self.flow_level > 0 && !d.flow_plain_allowed)
                || (self.flow_level == 0 && !d.block_plain_allowed)
            {
                style = ScalarStyle::SingleQuoted;
            }
            if d.value.is_empty() && (self.flow_level > 0 || self.simple_key_context) {
                style = ScalarStyle::SingleQuoted;
            }
            if no_tag && !plain_implicit {
                style = ScalarStyle::SingleQuoted;
            }
        }
        if style == ScalarStyle::SingleQuoted && !d.single_quoted_allowed {
            style = ScalarStyle::DoubleQuoted;
        }
        if (style == ScalarStyle::Literal || style == ScalarStyle::Folded)
            && (!d.block_allowed || self.flow_level > 0 || self.simple_key_context)
        {
            style = ScalarStyle::DoubleQuoted;
        }
        if no_tag && !quoted_implicit && style != ScalarStyle::Plain {
            self.tag_handle = Some("!");
        }
        self.scalar_data.style = Some(style);
    }

    /// `yaml_emitter_process_tag`.
    fn process_tag(&mut self) {
        if self.tag_handle.is_none() && self.tag_suffix.is_none() {
            return;
        }
        if let Some(handle) = self.tag_handle {
            self.write_tag_handle(handle.as_bytes());
            if let Some(suffix) = self.tag_suffix.clone() {
                self.write_tag_content(suffix.as_bytes(), false);
            }
        } else if let Some(suffix) = self.tag_suffix.clone() {
            self.write_indicator(b"!<", true, false, false);
            self.write_tag_content(suffix.as_bytes(), false);
            self.write_indicator(b">", false, false, false);
        }
    }

    fn process_scalar(&mut self) {
        let value = std::mem::take(&mut self.scalar_data.value);
        let allow_breaks = !self.simple_key_context;
        match self.scalar_data.style.unwrap_or(ScalarStyle::Plain) {
            ScalarStyle::Plain => self.write_plain_scalar(&value, allow_breaks),
            ScalarStyle::SingleQuoted => self.write_single_quoted_scalar(&value, allow_breaks),
            ScalarStyle::DoubleQuoted => self.write_double_quoted_scalar(&value, allow_breaks),
            ScalarStyle::Literal => self.write_literal_scalar(&value),
            ScalarStyle::Folded => self.write_folded_scalar(&value),
        }
        self.scalar_data.value = value;
    }

    fn write_indent(&mut self) {
        let indent = self.indent.max(0);
        if !self.indention || self.column > indent || (self.column == indent && !self.whitespace) {
            self.put_break();
        }
        while self.column < indent {
            self.put(b' ');
        }
        self.whitespace = true;
        self.indention = true;
    }

    fn write_indicator(
        &mut self,
        indicator: &[u8],
        need_whitespace: bool,
        is_whitespace: bool,
        is_indention: bool,
    ) {
        if need_whitespace && !self.whitespace {
            self.put(b' ');
        }
        let mut p = 0;
        while p < indicator.len() {
            self.write(indicator, &mut p);
        }
        self.whitespace = is_whitespace;
        self.indention = self.indention && is_indention;
        self.open_ended = false;
    }

    fn write_tag_handle(&mut self, value: &[u8]) {
        if !self.whitespace {
            self.put(b' ');
        }
        let mut p = 0;
        while p < value.len() {
            self.write(value, &mut p);
        }
        self.whitespace = false;
        self.indention = false;
    }

    fn write_tag_content(&mut self, value: &[u8], need_whitespace: bool) {
        if need_whitespace && !self.whitespace {
            self.put(b' ');
        }
        let mut p = 0;
        while p < value.len() {
            let c = value[p];
            if is_alpha_at(value, p) || b";/?:@&=+$,_.~*'()[]".contains(&c) {
                self.write(value, &mut p);
            } else {
                let w = width_at(value, p).max(1);
                for &octet in &value[p..(p + w).min(value.len())] {
                    self.put(b'%');
                    self.put(hex_digit(octet >> 4));
                    self.put(hex_digit(octet & 0x0F));
                }
                p += w;
            }
        }
        self.whitespace = false;
        self.indention = false;
    }

    fn write_plain_scalar(&mut self, s: &[u8], allow_breaks: bool) {
        let mut spaces = false;
        let mut breaks = false;
        if !self.whitespace {
            self.put(b' ');
        }
        let mut p = 0;
        while p < s.len() {
            if is_space_at(s, p) {
                if allow_breaks
                    && !spaces
                    && self.column > self.best_width
                    && !is_space_at(s, p + 1)
                {
                    self.write_indent();
                    p += 1;
                } else {
                    self.write(s, &mut p);
                }
                spaces = true;
            } else if is_break_at(s, p) {
                if !breaks && at(s, p) == b'\n' {
                    self.put_break();
                }
                self.write_break(s, &mut p);
                self.indention = true;
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                self.write(s, &mut p);
                self.indention = false;
                spaces = false;
                breaks = false;
            }
        }
        self.whitespace = false;
        self.indention = false;
        if self.root_context {
            self.open_ended = true;
        }
    }

    fn write_single_quoted_scalar(&mut self, s: &[u8], allow_breaks: bool) {
        let mut spaces = false;
        let mut breaks = false;
        self.write_indicator(b"'", true, false, false);
        let mut p = 0;
        while p < s.len() {
            if is_space_at(s, p) {
                if allow_breaks
                    && !spaces
                    && self.column > self.best_width
                    && p != 0
                    && p != s.len() - 1
                    && !is_space_at(s, p + 1)
                {
                    self.write_indent();
                    p += 1;
                } else {
                    self.write(s, &mut p);
                }
                spaces = true;
            } else if is_break_at(s, p) {
                if !breaks && at(s, p) == b'\n' {
                    self.put_break();
                }
                self.write_break(s, &mut p);
                self.indention = true;
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                if at(s, p) == b'\'' {
                    self.put(b'\'');
                }
                self.write(s, &mut p);
                self.indention = false;
                spaces = false;
                breaks = false;
            }
        }
        if breaks {
            self.write_indent();
        }
        self.write_indicator(b"'", false, false, false);
        self.whitespace = false;
        self.indention = false;
    }

    fn write_double_quoted_scalar(&mut self, s: &[u8], allow_breaks: bool) {
        let mut spaces = false;
        self.write_indicator(b"\"", true, false, false);
        let mut p = 0;
        while p < s.len() {
            let c = at(s, p);
            if !is_printable_at(s, p)
                || (!self.unicode && c > 0x7F)
                || is_bom_at(s, p)
                || is_break_at(s, p)
                || c == b'"'
                || c == b'\\'
            {
                let width = width_at(s, p);
                let mut value: u32 = match width {
                    1 => (c & 0x7F) as u32,
                    2 => (c & 0x1F) as u32,
                    3 => (c & 0x0F) as u32,
                    4 => (c & 0x07) as u32,
                    _ => 0,
                };
                for k in 1..width {
                    value = (value << 6) + (at(s, p + k) & 0x3F) as u32;
                }
                p += width.max(1);
                self.put(b'\\');
                match value {
                    0x00 => self.put(b'0'),
                    0x07 => self.put(b'a'),
                    0x08 => self.put(b'b'),
                    0x09 => self.put(b't'),
                    0x0A => self.put(b'n'),
                    0x0B => self.put(b'v'),
                    0x0C => self.put(b'f'),
                    0x0D => self.put(b'r'),
                    0x1B => self.put(b'e'),
                    0x22 => self.put(b'"'),
                    0x5C => self.put(b'\\'),
                    0x85 => self.put(b'N'),
                    0xA0 => self.put(b'_'),
                    0x2028 => self.put(b'L'),
                    0x2029 => self.put(b'P'),
                    _ => {
                        let digits = if value <= 0xFF {
                            self.put(b'x');
                            2
                        } else if value <= 0xFFFF {
                            self.put(b'u');
                            4
                        } else {
                            self.put(b'U');
                            8
                        };
                        for k in (0..digits).rev() {
                            self.put(hex_digit(((value >> (k * 4)) & 0x0F) as u8));
                        }
                    }
                }
                spaces = false;
            } else if is_space_at(s, p) {
                if allow_breaks
                    && !spaces
                    && self.column > self.best_width
                    && p != 0
                    && p != s.len() - 1
                {
                    self.write_indent();
                    if is_space_at(s, p + 1) {
                        self.put(b'\\');
                    }
                    p += 1;
                } else {
                    self.write(s, &mut p);
                }
                spaces = true;
            } else {
                self.write(s, &mut p);
                spaces = false;
            }
        }
        self.write_indicator(b"\"", false, false, false);
        self.whitespace = false;
        self.indention = false;
    }

    fn write_block_scalar_hints(&mut self, s: &[u8]) {
        if is_space_at(s, 0) || is_break_at(s, 0) {
            let hint = [b'0' + self.best_indent as u8];
            self.write_indicator(&hint, false, false, false);
        }
        self.open_ended = false;
        let mut chomp: Option<&[u8]> = None;
        if s.is_empty() {
            chomp = Some(b"-");
        } else {
            let mut p = s.len();
            loop {
                p -= 1;
                if s[p] & 0xC0 != 0x80 {
                    break;
                }
            }
            if !is_break_at(s, p) {
                chomp = Some(b"-");
            } else if p == 0 {
                chomp = Some(b"+");
                self.open_ended = true;
            } else {
                loop {
                    p -= 1;
                    if s[p] & 0xC0 != 0x80 {
                        break;
                    }
                }
                if is_break_at(s, p) {
                    chomp = Some(b"+");
                    self.open_ended = true;
                }
            }
        }
        if let Some(chomp) = chomp {
            self.write_indicator(chomp, false, false, false);
        }
    }

    fn write_literal_scalar(&mut self, s: &[u8]) {
        let mut breaks = true;
        self.write_indicator(b"|", true, false, false);
        self.write_block_scalar_hints(s);
        self.put_break();
        self.indention = true;
        self.whitespace = true;
        let mut p = 0;
        while p < s.len() {
            if is_break_at(s, p) {
                self.write_break(s, &mut p);
                self.indention = true;
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                self.write(s, &mut p);
                self.indention = false;
                breaks = false;
            }
        }
    }

    fn write_folded_scalar(&mut self, s: &[u8]) {
        let mut breaks = true;
        let mut leading_spaces = true;
        self.write_indicator(b">", true, false, false);
        self.write_block_scalar_hints(s);
        self.put_break();
        self.indention = true;
        self.whitespace = true;
        let mut p = 0;
        while p < s.len() {
            if is_break_at(s, p) {
                if !breaks && !leading_spaces && at(s, p) == b'\n' {
                    let mut k = 0;
                    while is_break_at(s, p + k) {
                        k += width_at(s, p + k);
                    }
                    if !is_blankz_at(s, p + k) {
                        self.put_break();
                    }
                }
                self.write_break(s, &mut p);
                self.indention = true;
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                    leading_spaces = is_blank_at(s, p);
                }
                if !breaks
                    && is_space_at(s, p)
                    && !is_space_at(s, p + 1)
                    && self.column > self.best_width
                {
                    self.write_indent();
                    p += 1;
                } else {
                    self.write(s, &mut p);
                }
                self.indention = false;
                breaks = false;
            }
        }
    }
}

fn hex_digit(v: u8) -> u8 {
    if v < 10 { b'0' + v } else { b'A' + v - 10 }
}

/// `yaml_emitter_check_empty_sequence` / `check_empty_mapping`.
fn check_empty(events: &[Event], i: usize) -> bool {
    matches!(
        (&events[i], events.get(i + 1)),
        (Event::SequenceStart { .. }, Some(Event::SequenceEnd))
            | (Event::MappingStart { .. }, Some(Event::MappingEnd))
    )
}
