//! JMESPath evaluation over [`Value`], used by `--query`, pagination, and
//! waiters.
//!
//! A port of jmespath.py 1.0.1, the version the reference AWS CLI bundles:
//! `jmespath/lexer.py` (`Lexer`), `jmespath/parser.py` (`Parser`),
//! `jmespath/visitor.py` (`TreeInterpreter`), `jmespath/functions.py`
//! (`Functions`), and `jmespath/exceptions.py`. Results keep Python's value
//! semantics: dict insertion order, `int` / `float` results, Python `==`
//! and ordering between numbers, and the exception messages (`str(e)`).

use std::cmp::Ordering;
use std::fmt;

use anyhow::Result;
use num_bigint::BigInt;
use num_traits::{FromPrimitive, Signed, ToPrimitive, Zero};

use crate::pyfmt;
use crate::value::{Map, Value};

/// A JMESPath error carrying the Python exception class name and
/// `str(exception)`.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    class: &'static str,
    message: String,
}

impl Error {
    fn new(class: &'static str, message: impl Into<String>) -> Error {
        Error {
            class,
            message: message.into(),
        }
    }

    /// The Python exception class (`ParseError`, `LexerError`,
    /// `JMESPathTypeError`, `TypeError`, ...).
    pub fn class(&self) -> &'static str {
        self.class
    }

    /// Whether the Python exception is a `ValueError` (every
    /// `JMESPathError` is one).
    pub fn is_value_error(&self) -> bool {
        !matches!(
            self.class,
            "TypeError" | "OverflowError" | "NotImplementedError"
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// A compiled JMESPath expression (`jmespath.parser.ParsedResult`).
pub struct Expression {
    source: String,
    node: Node,
}

impl Expression {
    /// `jmespath.compile`; errors carry the Python exception message.
    pub fn compile(source: &str) -> Result<Expression> {
        let node = Parser::parse(source)?;
        Ok(Expression {
            source: source.to_string(),
            node,
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// `ParsedResult.search`.
    pub fn search(&self, data: &Value) -> Result<Value> {
        Ok(visit(&self.node, data)?)
    }
}

pub fn search(expression: &str, data: &Value) -> Result<Value> {
    Expression::compile(expression)?.search(data)
}

/// botocore `utils.set_value_from_jmespath`: assign `value` at a dotted
/// key path, creating intermediate dicts.
pub fn set_value_from_jmespath(
    source: &mut Map,
    expression: &str,
    value: Value,
) -> Result<(), String> {
    let (head, rest) = match expression.split_once('.') {
        Some((h, r)) => (h, Some(r)),
        None => (expression, None),
    };
    match rest {
        Some(rest) => {
            let entry = source.entry(head.to_string()).or_insert_with(Value::map);
            set_value_in(entry, rest, value)
        }
        None => {
            source.insert(head.to_string(), value);
            Ok(())
        }
    }
}

/// The recursive step of `set_value_from_jmespath` on a value that may not
/// be a dict, raising the `TypeError` Python raises for it.
fn set_value_in(source: &mut Value, expression: &str, value: Value) -> Result<(), String> {
    let (head, rest) = match expression.split_once('.') {
        Some((h, r)) => (h, Some(r)),
        None => (expression, None),
    };
    if let Value::Map(m) = source {
        return match rest {
            Some(rest) => {
                let entry = m.entry(head.to_string()).or_insert_with(Value::map);
                set_value_in(entry, rest, value)
            }
            None => {
                m.insert(head.to_string(), value);
                Ok(())
            }
        };
    }
    let type_name = match source {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Str(_) => "str",
        Value::Bytes(_) => "bytes",
        Value::List(_) => "list",
        Value::Map(_) => unreachable!("handled above"),
    };
    if rest.is_some() {
        match source {
            Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) => {
                return Err(format!(
                    "argument of type '{type_name}' is not a container or iterable"
                ));
            }
            Value::Bytes(_) => return Err("a bytes-like object is required, not 'str'".to_string()),
            Value::Str(s) if s.contains(head) => {
                return Err("string indices must be integers, not 'str'".to_string());
            }
            Value::List(_) => {
                return Err("list indices must be integers or slices, not str".to_string());
            }
            _ => {}
        }
    }
    Err(match source {
        Value::List(_) => "list indices must be integers or slices, not str".to_string(),
        _ => format!("'{type_name}' object does not support item assignment"),
    })
}

// ---------------------------------------------------------------------------
// Lexer (`jmespath/lexer.py`)

#[derive(Debug, Clone)]
enum TokenValue {
    Str(String),
    Num(BigInt),
    Literal(Value),
}

impl TokenValue {
    /// `'%s' % token['value']`.
    fn display(&self) -> String {
        match self {
            TokenValue::Str(s) => s.clone(),
            TokenValue::Num(n) => n.to_string(),
            TokenValue::Literal(v) => pyfmt::py_str(v),
        }
    }
}

#[derive(Debug, Clone)]
struct Token {
    kind: &'static str,
    value: TokenValue,
    start: usize,
}

/// `jmespath.lexer.Lexer`. Positions count code points like Python string
/// indices.
struct Lexer<'a> {
    expression: &'a str,
    chars: Vec<char>,
    position: usize,
    current: Option<char>,
}

fn simple_token(c: char) -> Option<&'static str> {
    Some(match c {
        '.' => "dot",
        '*' => "star",
        ']' => "rbracket",
        ',' => "comma",
        ':' => "colon",
        '@' => "current",
        '(' => "lparen",
        ')' => "rparen",
        '{' => "lbrace",
        '}' => "rbrace",
        _ => return None,
    })
}

fn is_start_identifier(c: Option<char>) -> bool {
    matches!(c, Some(c) if c.is_ascii_alphabetic() || c == '_')
}

fn is_valid_identifier(c: Option<char>) -> bool {
    matches!(c, Some(c) if c.is_ascii_alphanumeric() || c == '_')
}

fn is_digit(c: Option<char>) -> bool {
    matches!(c, Some(c) if c.is_ascii_digit())
}

/// `exceptions.LexerError.__str__`.
fn lexer_error(expression: &str, position: usize, message: &str) -> Error {
    Error::new(
        "LexerError",
        format!(
            "Bad jmespath expression: {message}:\n{expression}\n{}^",
            " ".repeat(position)
        ),
    )
}

impl<'a> Lexer<'a> {
    /// `Lexer.tokenize`, consumed into a list as `Parser._parse` does.
    fn tokenize(expression: &'a str) -> Result<Vec<Token>, Error> {
        if expression.is_empty() {
            return Err(Error::new(
                "EmptyExpressionError",
                "Invalid JMESPath expression: cannot be empty.",
            ));
        }
        let chars: Vec<char> = expression.chars().collect();
        let mut lexer = Lexer {
            expression,
            current: Some(chars[0]),
            chars,
            position: 0,
        };
        lexer.run()
    }

    fn token(kind: &'static str, value: impl Into<String>, start: usize) -> Token {
        Token {
            kind,
            value: TokenValue::Str(value.into()),
            start,
        }
    }

    fn run(&mut self) -> Result<Vec<Token>, Error> {
        let mut tokens = Vec::new();
        while let Some(c) = self.current {
            if let Some(kind) = simple_token(c) {
                tokens.push(Self::token(kind, c, self.position));
                self.next();
            } else if is_start_identifier(Some(c)) {
                let start = self.position;
                let mut buff = String::from(c);
                while is_valid_identifier(self.next()) {
                    buff.push(self.current.expect("identifier char"));
                }
                tokens.push(Self::token("unquoted_identifier", buff, start));
            } else if matches!(c, ' ' | '\t' | '\n' | '\r') {
                self.next();
            } else if c == '[' {
                let start = self.position;
                match self.next() {
                    Some(']') => {
                        self.next();
                        tokens.push(Self::token("flatten", "[]", start));
                    }
                    Some('?') => {
                        self.next();
                        tokens.push(Self::token("filter", "[?", start));
                    }
                    _ => tokens.push(Self::token("lbracket", "[", start)),
                }
            } else if c == '\'' {
                tokens.push(self.consume_raw_string_literal()?);
            } else if c == '|' {
                tokens.push(self.match_or_else('|', "or", "pipe"));
            } else if c == '&' {
                tokens.push(self.match_or_else('&', "and", "expref"));
            } else if c == '`' {
                tokens.push(self.consume_literal()?);
            } else if c.is_ascii_digit() {
                let start = self.position;
                let buff = self.consume_number();
                tokens.push(Token {
                    kind: "number",
                    value: TokenValue::Num(buff.parse().expect("digits")),
                    start,
                });
            } else if c == '-' {
                let start = self.position;
                let buff = self.consume_number();
                if buff.chars().count() > 1 {
                    tokens.push(Token {
                        kind: "number",
                        value: TokenValue::Num(buff.parse().expect("digits")),
                        start,
                    });
                } else {
                    return Err(lexer_error(
                        self.expression,
                        start,
                        &format!("Unknown token '{buff}'"),
                    ));
                }
            } else if c == '"' {
                tokens.push(self.consume_quoted_identifier()?);
            } else if c == '<' {
                tokens.push(self.match_or_else('=', "lte", "lt"));
            } else if c == '>' {
                tokens.push(self.match_or_else('=', "gte", "gt"));
            } else if c == '!' {
                tokens.push(self.match_or_else('=', "ne", "not"));
            } else if c == '=' {
                if self.next() == Some('=') {
                    tokens.push(Self::token("eq", "==", self.position - 1));
                    self.next();
                } else {
                    let position = if self.current.is_none() {
                        self.position
                    } else {
                        self.position - 1
                    };
                    return Err(lexer_error(self.expression, position, "Unknown token '='"));
                }
            } else {
                return Err(lexer_error(
                    self.expression,
                    self.position,
                    &format!("Unknown token {c}"),
                ));
            }
        }
        tokens.push(Self::token("eof", "", self.chars.len()));
        Ok(tokens)
    }

    fn consume_number(&mut self) -> String {
        let mut buff = String::from(self.current.expect("number start"));
        while is_digit(self.next()) {
            buff.push(self.current.expect("digit"));
        }
        buff
    }

    fn next(&mut self) -> Option<char> {
        if self.position == self.chars.len() - 1 {
            self.current = None;
        } else {
            self.position += 1;
            self.current = Some(self.chars[self.position]);
        }
        self.current
    }

    /// `Lexer._consume_until`: consume up to `delimiter`, keeping `\`
    /// escapes verbatim.
    fn consume_until(&mut self, delimiter: char) -> Result<String, Error> {
        let start = self.position;
        let mut buff = String::new();
        self.next();
        while self.current != Some(delimiter) {
            if self.current == Some('\\') {
                buff.push('\\');
                self.next();
            }
            match self.current {
                None => {
                    return Err(lexer_error(
                        self.expression,
                        start,
                        &format!("Unclosed {delimiter} delimiter"),
                    ));
                }
                Some(c) => buff.push(c),
            }
            self.next();
        }
        self.next();
        Ok(buff)
    }

    fn consume_literal(&mut self) -> Result<Token, Error> {
        let start = self.position;
        let lexeme = self.consume_until('`')?.replace("\\`", "`");
        let parsed = match py_json_loads(&lexeme) {
            Ok(v) => v,
            Err(_) => match py_json_loads(&format!("\"{}\"", py_lstrip(&lexeme))) {
                Ok(v) => v,
                Err(_) => {
                    return Err(lexer_error(
                        self.expression,
                        start,
                        &format!("Bad token {lexeme}"),
                    ));
                }
            },
        };
        Ok(Token {
            kind: "literal",
            value: TokenValue::Literal(parsed),
            start,
        })
    }

    fn consume_quoted_identifier(&mut self) -> Result<Token, Error> {
        let start = self.position;
        let lexeme = format!("\"{}\"", self.consume_until('"')?);
        match py_json_loads(&lexeme) {
            Ok(Value::Str(s)) => Ok(Self::token("quoted_identifier", s, start)),
            Ok(other) => Ok(Token {
                kind: "quoted_identifier",
                value: TokenValue::Literal(other),
                start,
            }),
            Err(e) => {
                let message = e.split(':').next().unwrap_or_default().to_string();
                Err(lexer_error(self.expression, start, &message))
            }
        }
    }

    fn consume_raw_string_literal(&mut self) -> Result<Token, Error> {
        let start = self.position;
        let lexeme = self.consume_until('\'')?.replace("\\'", "'");
        Ok(Token {
            kind: "literal",
            value: TokenValue::Literal(Value::Str(lexeme)),
            start,
        })
    }

    fn match_or_else(
        &mut self,
        expected: char,
        match_type: &'static str,
        else_type: &'static str,
    ) -> Token {
        let start = self.position;
        let current = self.current.expect("current char");
        let next_char = self.next();
        if next_char == Some(expected) {
            self.next();
            return Self::token(match_type, format!("{current}{expected}"), start);
        }
        Self::token(else_type, current, start)
    }
}

/// Python `str.isspace` for one character.
fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn py_lstrip(s: &str) -> &str {
    s.trim_start_matches(py_isspace)
}

fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

// ---------------------------------------------------------------------------
// Python `json.loads` (`json/decoder.py`, `Modules/_json.c`), used for
// literals and quoted identifiers. Errors are the `JSONDecodeError`
// message (`msg: line L column C (char N)`); only the part before `:`
// is used by the lexer.

fn py_json_loads(s: &str) -> Result<Value, String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.first() == Some(&'\u{feff}') {
        return Err(json_error(
            "Unexpected UTF-8 BOM (decode using utf-8-sig)",
            &chars,
            0,
        ));
    }
    let start = skip_json_ws(&chars, 0);
    let (value, end) = match json_scan_once(&chars, start)? {
        Some(r) => r,
        None => return Err(json_error("Expecting value", &chars, start)),
    };
    let end = skip_json_ws(&chars, end);
    if end != chars.len() {
        return Err(json_error("Extra data", &chars, end));
    }
    Ok(value)
}

/// `json.JSONDecodeError.__init__` message.
fn json_error(msg: &str, chars: &[char], pos: usize) -> String {
    let lineno = chars[..pos].iter().filter(|c| **c == '\n').count() + 1;
    let colno = match chars[..pos].iter().rposition(|c| *c == '\n') {
        Some(nl) => pos - nl,
        None => pos + 1,
    };
    format!("{msg}: line {lineno} column {colno} (char {pos})")
}

fn skip_json_ws(chars: &[char], mut idx: usize) -> usize {
    while idx < chars.len() && matches!(chars[idx], ' ' | '\t' | '\n' | '\r') {
        idx += 1;
    }
    idx
}

/// `scanner.scan_once`; `Ok(None)` is `StopIteration`.
fn json_scan_once(chars: &[char], idx: usize) -> Result<Option<(Value, usize)>, String> {
    let Some(&c) = chars.get(idx) else {
        return Ok(None);
    };
    let rest_starts = |lit: &str| {
        let lit: Vec<char> = lit.chars().collect();
        chars.len() >= idx + lit.len() && chars[idx..idx + lit.len()] == lit[..]
    };
    match c {
        '"' => {
            let (s, end) = json_scanstring(chars, idx + 1)?;
            Ok(Some((Value::Str(s), end)))
        }
        '{' => json_parse_object(chars, idx + 1).map(Some),
        '[' => json_parse_array(chars, idx + 1).map(Some),
        'n' if rest_starts("null") => Ok(Some((Value::Null, idx + 4))),
        't' if rest_starts("true") => Ok(Some((Value::Bool(true), idx + 4))),
        'f' if rest_starts("false") => Ok(Some((Value::Bool(false), idx + 5))),
        'N' if rest_starts("NaN") => Ok(Some((Value::Float(f64::NAN), idx + 3))),
        'I' if rest_starts("Infinity") => Ok(Some((Value::Float(f64::INFINITY), idx + 8))),
        '-' if rest_starts("-Infinity") => Ok(Some((Value::Float(f64::NEG_INFINITY), idx + 9))),
        _ => Ok(json_match_number(chars, idx)),
    }
}

/// `_match_number_unicode`.
fn json_match_number(chars: &[char], start: usize) -> Option<(Value, usize)> {
    let digit = |i: usize| chars.get(i).is_some_and(|c| c.is_ascii_digit());
    let mut idx = start;
    if chars.get(idx) == Some(&'-') {
        idx += 1;
    }
    match chars.get(idx) {
        Some('1'..='9') => {
            idx += 1;
            while digit(idx) {
                idx += 1;
            }
        }
        Some('0') => idx += 1,
        _ => return None,
    }
    let mut is_float = false;
    if chars.get(idx) == Some(&'.') && digit(idx + 1) {
        is_float = true;
        idx += 2;
        while digit(idx) {
            idx += 1;
        }
    }
    if matches!(chars.get(idx), Some('e' | 'E')) {
        let save = idx;
        idx += 1;
        if matches!(chars.get(idx), Some('-' | '+')) {
            idx += 1;
        }
        let digits_start = idx;
        while digit(idx) {
            idx += 1;
        }
        if idx > digits_start {
            is_float = true;
        } else {
            idx = save;
        }
    }
    let text: String = chars[start..idx].iter().collect();
    let value = if is_float {
        Value::Float(text.parse().expect("float lexeme"))
    } else {
        Value::Int(text.parse().expect("int lexeme"))
    };
    Some((value, idx))
}

/// `scanstring_unicode` (strict): `end` is the index after the opening
/// quote. Lone surrogates, which a Rust string cannot hold, become
/// U+FFFD.
fn json_scanstring(chars: &[char], end: usize) -> Result<(String, usize), String> {
    let begin = end - 1;
    let mut out = String::new();
    let mut next = end;
    loop {
        let mut terminator = None;
        while next < chars.len() {
            let c = chars[next];
            if c == '"' || c == '\\' {
                terminator = Some(c);
                break;
            }
            if (c as u32) <= 0x1f {
                return Err(json_error("Invalid control character at", chars, next));
            }
            out.push(c);
            next += 1;
        }
        let Some(term) = terminator else {
            return Err(json_error("Unterminated string starting at", chars, begin));
        };
        next += 1;
        if term == '"' {
            return Ok((out, next));
        }
        let Some(&esc) = chars.get(next) else {
            return Err(json_error("Unterminated string starting at", chars, begin));
        };
        if esc != 'u' {
            let c = match esc {
                '"' | '\\' | '/' => esc,
                'b' => '\u{8}',
                'f' => '\u{c}',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                _ => return Err(json_error("Invalid \\escape", chars, next - 1)),
            };
            out.push(c);
            next += 1;
            continue;
        }
        next += 1;
        let mut end = next + 4;
        if end >= chars.len() {
            return Err(json_error("Invalid \\uXXXX escape", chars, next - 1));
        }
        let hex = |from: usize| -> Option<u32> {
            chars[from..from + 4]
                .iter()
                .try_fold(0u32, |acc, c| Some(acc * 16 + c.to_digit(16)?))
        };
        let Some(mut code) = hex(next) else {
            return Err(json_error("Invalid \\uXXXX escape", chars, end - 5));
        };
        if (0xd800..0xdc00).contains(&code)
            && end + 6 < chars.len()
            && chars[end] == '\\'
            && chars[end + 1] == 'u'
        {
            let Some(low) = hex(end + 2) else {
                return Err(json_error("Invalid \\uXXXX escape", chars, end - 5));
            };
            if (0xdc00..0xe000).contains(&low) {
                code = 0x10000 + (((code - 0xd800) << 10) | (low - 0xdc00));
                end += 6;
            }
        }
        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
        next = end;
    }
}

/// `_parse_object_unicode`.
fn json_parse_object(chars: &[char], start: usize) -> Result<(Value, usize), String> {
    let mut map = Map::new();
    let mut idx = skip_json_ws(chars, start);
    if chars.get(idx) == Some(&'}') {
        return Ok((Value::Map(map), idx + 1));
    }
    loop {
        if chars.get(idx) != Some(&'"') {
            return Err(json_error(
                "Expecting property name enclosed in double quotes",
                chars,
                idx,
            ));
        }
        let (key, end) = json_scanstring(chars, idx + 1)?;
        idx = skip_json_ws(chars, end);
        if chars.get(idx) != Some(&':') {
            return Err(json_error("Expecting ':' delimiter", chars, idx));
        }
        idx = skip_json_ws(chars, idx + 1);
        let Some((value, end)) = json_scan_once(chars, idx)? else {
            return Err(json_error("Expecting value", chars, idx));
        };
        map.insert(key, value);
        idx = skip_json_ws(chars, end);
        match chars.get(idx) {
            Some('}') => return Ok((Value::Map(map), idx + 1)),
            Some(',') => {}
            _ => return Err(json_error("Expecting ',' delimiter", chars, idx)),
        }
        let comma = idx;
        idx = skip_json_ws(chars, idx + 1);
        if chars.get(idx) == Some(&'}') {
            return Err(json_error(
                "Illegal trailing comma before end of object",
                chars,
                comma,
            ));
        }
    }
}

/// `_parse_array_unicode`.
fn json_parse_array(chars: &[char], start: usize) -> Result<(Value, usize), String> {
    let mut list = Vec::new();
    let mut idx = skip_json_ws(chars, start);
    if chars.get(idx) == Some(&']') {
        return Ok((Value::List(list), idx + 1));
    }
    loop {
        let Some((value, end)) = json_scan_once(chars, idx)? else {
            return Err(json_error("Expecting value", chars, idx));
        };
        list.push(value);
        idx = skip_json_ws(chars, end);
        match chars.get(idx) {
            Some(']') => return Ok((Value::List(list), idx + 1)),
            Some(',') => {}
            _ => return Err(json_error("Expecting ',' delimiter", chars, idx)),
        }
        let comma = idx;
        idx = skip_json_ws(chars, idx + 1);
        if chars.get(idx) == Some(&']') {
            return Err(json_error(
                "Illegal trailing comma before end of array",
                chars,
                comma,
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// AST (`jmespath/ast.py`) and parser (`jmespath/parser.py`)

#[derive(Debug, Clone)]
enum Node {
    Field(String),
    Subexpression(Vec<Node>),
    Comparator(&'static str, Box<Node>, Box<Node>),
    Current,
    Expref(Box<Node>),
    Function(String, Vec<Node>),
    /// left, right, condition
    FilterProjection(Box<Node>, Box<Node>, Box<Node>),
    Flatten(Box<Node>),
    Identity,
    Index(BigInt),
    IndexExpression(Vec<Node>),
    Slice(Option<BigInt>, Option<BigInt>, Option<BigInt>),
    Literal(Value),
    MultiSelectDict(Vec<(String, Node)>),
    MultiSelectList(Vec<Node>),
    Or(Box<Node>, Box<Node>),
    And(Box<Node>, Box<Node>),
    Not(Box<Node>),
    Pipe(Box<Node>, Box<Node>),
    Projection(Box<Node>, Box<Node>),
    ValueProjection(Box<Node>, Box<Node>),
}

/// `Parser.BINDING_POWER`.
fn binding_power(kind: &str) -> u8 {
    match kind {
        "pipe" => 1,
        "or" => 2,
        "and" => 3,
        "eq" | "gt" | "lt" | "gte" | "lte" | "ne" => 5,
        "flatten" => 9,
        "star" => 20,
        "filter" => 21,
        "dot" => 40,
        "not" => 45,
        "lbrace" => 50,
        "lbracket" => 55,
        "lparen" => 60,
        _ => 0,
    }
}

/// `Parser._PROJECTION_STOP`.
const PROJECTION_STOP: u8 = 10;

/// `jmespath.parser.Parser` (a Pratt parser).
struct Parser<'a> {
    expression: &'a str,
    tokens: Vec<Token>,
    index: usize,
}

impl<'a> Parser<'a> {
    /// `Parser._do_parse` / `Parser._parse`.
    fn parse(expression: &'a str) -> Result<Node, Error> {
        let tokens = Lexer::tokenize(expression)?;
        let mut parser = Parser {
            expression,
            tokens,
            index: 0,
        };
        let parsed = parser.expression(0)?;
        if parser.current_token() != "eof" {
            let t = parser.lookahead_token(0).clone();
            let msg = format!("Unexpected token: {}", t.value.display());
            return Err(parser.parse_error(&t, &msg));
        }
        Ok(parsed)
    }

    /// `ParseError.__str__` (with the expression filled in by
    /// `_do_parse`).
    fn parse_error_at(&self, position: usize, value: &str, kind: &str, msg: &str) -> Error {
        Error::new(
            "ParseError",
            format!(
                "{msg}: Parse error at column {position}, token \"{value}\" ({}), for expression:\n\"{}\"\n{}^",
                kind.to_uppercase(),
                self.expression,
                " ".repeat(position + 1)
            ),
        )
    }

    fn parse_error(&self, token: &Token, msg: &str) -> Error {
        self.parse_error_at(token.start, &token.value.display(), token.kind, msg)
    }

    /// `IncompleteExpressionError.__str__` after `set_expression`.
    fn incomplete(&self) -> Error {
        let len = self.expression.chars().count();
        Error::new(
            "IncompleteExpressionError",
            format!(
                "Invalid jmespath expression: Incomplete expression:\n\"{}\"\n{}^",
                self.expression,
                " ".repeat(len + 1)
            ),
        )
    }

    fn expression(&mut self, bp: u8) -> Result<Node, Error> {
        let left_token = self.lookahead_token(0).clone();
        self.advance();
        let mut left = self.nud(&left_token)?;
        let mut current = self.current_token();
        while bp < binding_power(current) {
            match current {
                "dot" | "pipe" | "or" | "and" | "lparen" | "filter" | "eq" | "ne" | "gt"
                | "gte" | "lt" | "lte" | "flatten" | "lbracket" => {
                    self.advance();
                    left = self.led(current, left)?;
                    current = self.current_token();
                }
                _ => {
                    let t = self.lookahead_token(0).clone();
                    return Err(self.parse_error(&t, "invalid token"));
                }
            }
        }
        Ok(left)
    }

    fn nud(&mut self, token: &Token) -> Result<Node, Error> {
        match token.kind {
            "literal" => match &token.value {
                TokenValue::Literal(v) => Ok(Node::Literal(v.clone())),
                other => Ok(Node::Literal(Value::Str(other.display()))),
            },
            "unquoted_identifier" => Ok(Node::Field(token.value.display())),
            "quoted_identifier" => {
                if self.current_token() == "lparen" {
                    let t = self.lookahead_token(0).clone();
                    return Err(self.parse_error_at(
                        0,
                        &t.value.display(),
                        t.kind,
                        "Quoted identifier not allowed for function names.",
                    ));
                }
                Ok(Node::Field(token.value.display()))
            }
            "star" => {
                let right = if self.current_token() == "rbracket" {
                    Node::Identity
                } else {
                    self.parse_projection_rhs(binding_power("star"))?
                };
                Ok(Node::ValueProjection(
                    Box::new(Node::Identity),
                    Box::new(right),
                ))
            }
            "filter" => self.led_filter(Node::Identity),
            "lbrace" => self.parse_multi_select_hash(),
            "lparen" => {
                let expression = self.expression(0)?;
                self.match_token("rparen")?;
                Ok(expression)
            }
            "flatten" => {
                let right = self.parse_projection_rhs(binding_power("flatten"))?;
                Ok(Node::Projection(
                    Box::new(Node::Flatten(Box::new(Node::Identity))),
                    Box::new(right),
                ))
            }
            "not" => {
                let expr = self.expression(binding_power("not"))?;
                Ok(Node::Not(Box::new(expr)))
            }
            "lbracket" => {
                let current = self.current_token();
                if current == "number" || current == "colon" {
                    let right = self.parse_index_expression()?;
                    self.project_if_slice(Node::Identity, right)
                } else if current == "star" && self.lookahead(1) == "rbracket" {
                    self.advance();
                    self.advance();
                    let right = self.parse_projection_rhs(binding_power("star"))?;
                    Ok(Node::Projection(Box::new(Node::Identity), Box::new(right)))
                } else {
                    self.parse_multi_select_list()
                }
            }
            "current" => Ok(Node::Current),
            "expref" => {
                let expression = self.expression(binding_power("expref"))?;
                Ok(Node::Expref(Box::new(expression)))
            }
            "eof" => Err(self.incomplete()),
            _ => Err(self.parse_error(token, "invalid token")),
        }
    }

    fn led(&mut self, kind: &str, left: Node) -> Result<Node, Error> {
        match kind {
            "dot" => {
                if self.current_token() != "star" {
                    let right = self.parse_dot_rhs(binding_power("dot"))?;
                    match left {
                        Node::Subexpression(mut children) => {
                            children.push(right);
                            Ok(Node::Subexpression(children))
                        }
                        left => Ok(Node::Subexpression(vec![left, right])),
                    }
                } else {
                    self.advance();
                    let right = self.parse_projection_rhs(binding_power("dot"))?;
                    Ok(Node::ValueProjection(Box::new(left), Box::new(right)))
                }
            }
            "pipe" => {
                let right = self.expression(binding_power("pipe"))?;
                Ok(Node::Pipe(Box::new(left), Box::new(right)))
            }
            "or" => {
                let right = self.expression(binding_power("or"))?;
                Ok(Node::Or(Box::new(left), Box::new(right)))
            }
            "and" => {
                let right = self.expression(binding_power("and"))?;
                Ok(Node::And(Box::new(left), Box::new(right)))
            }
            "lparen" => {
                let Node::Field(name) = left else {
                    let prev = self.lookahead_token(-2).clone();
                    let msg = format!("Invalid function name '{}'", prev.value.display());
                    return Err(self.parse_error(&prev, &msg));
                };
                let mut args = Vec::new();
                while self.current_token() != "rparen" {
                    let expression = self.expression(0)?;
                    if self.current_token() == "comma" {
                        self.match_token("comma")?;
                    }
                    args.push(expression);
                }
                self.match_token("rparen")?;
                Ok(Node::Function(name, args))
            }
            "filter" => self.led_filter(left),
            "eq" | "ne" | "gt" | "gte" | "lt" | "lte" => {
                let op = match kind {
                    "eq" => "eq",
                    "ne" => "ne",
                    "gt" => "gt",
                    "gte" => "gte",
                    "lt" => "lt",
                    _ => "lte",
                };
                let right = self.expression(binding_power(op))?;
                Ok(Node::Comparator(op, Box::new(left), Box::new(right)))
            }
            "flatten" => {
                let right = self.parse_projection_rhs(binding_power("flatten"))?;
                Ok(Node::Projection(
                    Box::new(Node::Flatten(Box::new(left))),
                    Box::new(right),
                ))
            }
            _ => {
                let token = self.current_token();
                if token == "number" || token == "colon" {
                    let right = self.parse_index_expression()?;
                    match left {
                        Node::IndexExpression(mut children) => {
                            children.push(right);
                            Ok(Node::IndexExpression(children))
                        }
                        left => self.project_if_slice(left, right),
                    }
                } else {
                    self.match_token("star")?;
                    self.match_token("rbracket")?;
                    let right = self.parse_projection_rhs(binding_power("star"))?;
                    Ok(Node::Projection(Box::new(left), Box::new(right)))
                }
            }
        }
    }

    /// `Parser._token_led_filter`.
    fn led_filter(&mut self, left: Node) -> Result<Node, Error> {
        let condition = self.expression(0)?;
        self.match_token("rbracket")?;
        let right = if self.current_token() == "flatten" {
            Node::Identity
        } else {
            self.parse_projection_rhs(binding_power("filter"))?
        };
        Ok(Node::FilterProjection(
            Box::new(left),
            Box::new(right),
            Box::new(condition),
        ))
    }

    fn parse_index_expression(&mut self) -> Result<Node, Error> {
        if self.lookahead(0) == "colon" || self.lookahead(1) == "colon" {
            self.parse_slice_expression()
        } else {
            let node = match &self.lookahead_token(0).value {
                TokenValue::Num(n) => Node::Index(n.clone()),
                _ => unreachable!("index token is a number"),
            };
            self.advance();
            self.match_token("rbracket")?;
            Ok(node)
        }
    }

    fn parse_slice_expression(&mut self) -> Result<Node, Error> {
        let mut parts: [Option<BigInt>; 3] = [None, None, None];
        let mut index = 0;
        let mut current = self.current_token();
        while current != "rbracket" && index < 3 {
            if current == "colon" {
                index += 1;
                if index == 3 {
                    let t = self.lookahead_token(0).clone();
                    return Err(self.parse_error(&t, "syntax error"));
                }
                self.advance();
            } else if current == "number" {
                if let TokenValue::Num(n) = &self.lookahead_token(0).value {
                    parts[index] = Some(n.clone());
                }
                self.advance();
            } else {
                let t = self.lookahead_token(0).clone();
                return Err(self.parse_error(&t, "syntax error"));
            }
            current = self.current_token();
        }
        self.match_token("rbracket")?;
        let [start, stop, step] = parts;
        Ok(Node::Slice(start, stop, step))
    }

    fn project_if_slice(&mut self, left: Node, right: Node) -> Result<Node, Error> {
        let is_slice = matches!(right, Node::Slice(..));
        let index_expr = Node::IndexExpression(vec![left, right]);
        if is_slice {
            let rhs = self.parse_projection_rhs(binding_power("star"))?;
            Ok(Node::Projection(Box::new(index_expr), Box::new(rhs)))
        } else {
            Ok(index_expr)
        }
    }

    fn parse_multi_select_list(&mut self) -> Result<Node, Error> {
        let mut expressions = Vec::new();
        loop {
            expressions.push(self.expression(0)?);
            if self.current_token() == "rbracket" {
                break;
            }
            self.match_token("comma")?;
        }
        self.match_token("rbracket")?;
        Ok(Node::MultiSelectList(expressions))
    }

    fn parse_multi_select_hash(&mut self) -> Result<Node, Error> {
        let mut pairs = Vec::new();
        loop {
            let key_token = self.lookahead_token(0).clone();
            self.match_multiple_tokens(&["quoted_identifier", "unquoted_identifier"])?;
            let key_name = key_token.value.display();
            self.match_token("colon")?;
            let value = self.expression(0)?;
            pairs.push((key_name, value));
            if self.current_token() == "comma" {
                self.match_token("comma")?;
            } else if self.current_token() == "rbrace" {
                self.match_token("rbrace")?;
                break;
            }
        }
        Ok(Node::MultiSelectDict(pairs))
    }

    fn parse_projection_rhs(&mut self, bp: u8) -> Result<Node, Error> {
        let current = self.current_token();
        if binding_power(current) < PROJECTION_STOP {
            Ok(Node::Identity)
        } else if current == "lbracket" || current == "filter" {
            self.expression(bp)
        } else if current == "dot" {
            self.match_token("dot")?;
            self.parse_dot_rhs(bp)
        } else {
            let t = self.lookahead_token(0).clone();
            Err(self.parse_error(&t, "syntax error"))
        }
    }

    fn parse_dot_rhs(&mut self, bp: u8) -> Result<Node, Error> {
        match self.current_token() {
            "quoted_identifier" | "unquoted_identifier" | "star" => self.expression(bp),
            "lbracket" => {
                self.match_token("lbracket")?;
                self.parse_multi_select_list()
            }
            "lbrace" => {
                self.match_token("lbrace")?;
                self.parse_multi_select_hash()
            }
            _ => {
                let t = self.lookahead_token(0).clone();
                let msg = format!(
                    "Expecting: ['quoted_identifier', 'unquoted_identifier', 'lbracket', 'lbrace'], got: {}",
                    t.kind
                );
                Err(self.parse_error(&t, &msg))
            }
        }
    }

    fn match_token(&mut self, kind: &str) -> Result<(), Error> {
        if self.current_token() == kind {
            self.advance();
            Ok(())
        } else {
            Err(self.parse_error_maybe_eof(kind))
        }
    }

    fn match_multiple_tokens(&mut self, kinds: &[&str]) -> Result<(), Error> {
        if !kinds.contains(&self.current_token()) {
            let expected = format!(
                "[{}]",
                kinds
                    .iter()
                    .map(|k| format!("'{k}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Err(self.parse_error_maybe_eof(&expected));
        }
        self.advance();
        Ok(())
    }

    fn parse_error_maybe_eof(&self, expected: &str) -> Error {
        let token = self.lookahead_token(0);
        if token.kind == "eof" {
            return self.incomplete();
        }
        let msg = format!("Expecting: {expected}, got: {}", token.kind);
        self.parse_error(token, &msg)
    }

    fn advance(&mut self) {
        self.index += 1;
    }

    fn current_token(&self) -> &'static str {
        self.lookahead(0)
    }

    fn lookahead(&self, number: isize) -> &'static str {
        self.lookahead_token(number).kind
    }

    fn lookahead_token(&self, number: isize) -> &Token {
        let index = self.index as isize + number;
        let last = self.tokens.len() - 1;
        &self.tokens[index.clamp(0, last as isize) as usize]
    }
}

// ---------------------------------------------------------------------------
// Interpreter (`jmespath/visitor.py`)

/// A resolved function argument: a value, or an `_Expression` (`&expr`).
enum Arg<'a> {
    Value(Value),
    Expref(&'a Node),
}

fn expref_unsupported() -> Error {
    Error::new(
        "NotImplementedError",
        "awsr: a JMESPath expression reference (&expr) is only supported as a function argument",
    )
}

/// `TreeInterpreter.visit`.
fn visit(node: &Node, value: &Value) -> Result<Value, Error> {
    match node {
        Node::Subexpression(children) | Node::IndexExpression(children) => {
            let mut result = value.clone();
            for child in children {
                result = visit(child, &result)?;
            }
            Ok(result)
        }
        Node::Pipe(left, right) => {
            let result = visit(left, value)?;
            visit(right, &result)
        }
        Node::Field(key) => Ok(match value {
            Value::Map(m) => m.get(key).cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        }),
        Node::Comparator(op, left, right) => {
            let left = visit(left, value)?;
            let right = visit(right, value)?;
            match *op {
                "eq" => Ok(Value::Bool(jmes_equals(&left, &right))),
                "ne" => Ok(Value::Bool(!jmes_equals(&left, &right))),
                _ => {
                    if !(is_comparable(&left) && is_comparable(&right)) {
                        return Ok(Value::Null);
                    }
                    let symbol = match *op {
                        "lt" => "<",
                        "lte" => "<=",
                        "gt" => ">",
                        _ => ">=",
                    };
                    Ok(Value::Bool(py_compare(symbol, &left, &right)?))
                }
            }
        }
        Node::Current | Node::Identity => Ok(value.clone()),
        Node::Expref(_) => Err(expref_unsupported()),
        Node::Function(name, children) => {
            let mut args = Vec::with_capacity(children.len());
            for child in children {
                args.push(match child {
                    Node::Expref(inner) => Arg::Expref(inner),
                    other => Arg::Value(visit(other, value)?),
                });
            }
            call_function(name, args)
        }
        Node::FilterProjection(left, right, condition) => {
            let Value::List(base) = visit(left, value)? else {
                return Ok(Value::Null);
            };
            let mut collected = Vec::new();
            for element in &base {
                if is_true(&visit(condition, element)?) {
                    let current = visit(right, element)?;
                    if current != Value::Null {
                        collected.push(current);
                    }
                }
            }
            Ok(Value::List(collected))
        }
        Node::Flatten(child) => {
            let Value::List(base) = visit(child, value)? else {
                return Ok(Value::Null);
            };
            let mut merged = Vec::new();
            for element in base {
                match element {
                    Value::List(l) => merged.extend(l),
                    other => merged.push(other),
                }
            }
            Ok(Value::List(merged))
        }
        Node::Index(i) => {
            let Value::List(l) = value else {
                return Ok(Value::Null);
            };
            Ok(py_index(l, i).cloned().unwrap_or(Value::Null))
        }
        Node::Slice(start, stop, step) => {
            let Value::List(l) = value else {
                return Ok(Value::Null);
            };
            py_slice(l, start.as_ref(), stop.as_ref(), step.as_ref()).map(Value::List)
        }
        Node::Literal(v) => Ok(v.clone()),
        Node::MultiSelectDict(pairs) => {
            if *value == Value::Null {
                return Ok(Value::Null);
            }
            let mut collected = Map::new();
            for (key, child) in pairs {
                collected.insert(key.clone(), visit(child, value)?);
            }
            Ok(Value::Map(collected))
        }
        Node::MultiSelectList(children) => {
            if *value == Value::Null {
                return Ok(Value::Null);
            }
            let mut collected = Vec::with_capacity(children.len());
            for child in children {
                collected.push(visit(child, value)?);
            }
            Ok(Value::List(collected))
        }
        Node::Or(left, right) => {
            let matched = visit(left, value)?;
            if is_false(&matched) {
                visit(right, value)
            } else {
                Ok(matched)
            }
        }
        Node::And(left, right) => {
            let matched = visit(left, value)?;
            if is_false(&matched) {
                Ok(matched)
            } else {
                visit(right, value)
            }
        }
        Node::Not(child) => {
            let original = visit(child, value)?;
            if is_actual_number(&original) && py_eq(&original, &Value::int(0)) {
                return Ok(Value::Bool(false));
            }
            Ok(Value::Bool(!original.is_truthy()))
        }
        Node::Projection(left, right) => {
            let Value::List(base) = visit(left, value)? else {
                return Ok(Value::Null);
            };
            project(base.iter(), right)
        }
        Node::ValueProjection(left, right) => {
            let Value::Map(base) = visit(left, value)? else {
                return Ok(Value::Null);
            };
            project(base.values(), right)
        }
    }
}

fn project<'v>(base: impl Iterator<Item = &'v Value>, right: &Node) -> Result<Value, Error> {
    let mut collected = Vec::new();
    for element in base {
        let current = visit(right, element)?;
        if current != Value::Null {
            collected.push(current);
        }
    }
    Ok(Value::List(collected))
}

/// `TreeInterpreter._is_false`.
fn is_false(value: &Value) -> bool {
    match value {
        Value::Str(s) => s.is_empty(),
        Value::List(l) => l.is_empty(),
        Value::Map(m) => m.is_empty(),
        Value::Null | Value::Bool(false) => true,
        _ => false,
    }
}

fn is_true(value: &Value) -> bool {
    !is_false(value)
}

/// `visitor._is_actual_number`.
fn is_actual_number(value: &Value) -> bool {
    matches!(value, Value::Int(_) | Value::Float(_))
}

/// `visitor._is_comparable`.
fn is_comparable(value: &Value) -> bool {
    is_actual_number(value) || matches!(value, Value::Str(_))
}

/// `visitor._equals`: Python `==`, except that `0` / `1` never equal a
/// boolean.
fn jmes_equals(x: &Value, y: &Value) -> bool {
    let zero_or_one =
        |v: &Value| is_actual_number(v) && (py_eq(v, &Value::int(0)) || py_eq(v, &Value::int(1)));
    let special = if zero_or_one(x) {
        matches!(y, Value::Bool(_))
    } else {
        zero_or_one(y) && matches!(x, Value::Bool(_))
    };
    !special && py_eq(x, y)
}

/// A number as Python sees it (`bool` is an `int` subclass).
enum Num<'a> {
    Int(std::borrow::Cow<'a, BigInt>),
    Float(f64),
}

fn as_num(v: &Value) -> Option<Num<'_>> {
    match v {
        Value::Int(i) => Some(Num::Int(std::borrow::Cow::Borrowed(i))),
        Value::Float(f) => Some(Num::Float(*f)),
        Value::Bool(b) => Some(Num::Int(std::borrow::Cow::Owned(BigInt::from(u8::from(
            *b,
        ))))),
        _ => None,
    }
}

/// Exact comparison of an `int` and a `float` (CPython `float_richcompare`).
fn int_float_cmp(i: &BigInt, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f.is_infinite() {
        return Some(if f > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let t = f.trunc();
    let ti = BigInt::from_f64(t).expect("finite float");
    Some(match i.cmp(&ti) {
        Ordering::Equal if f > t => Ordering::Less,
        Ordering::Equal if f < t => Ordering::Greater,
        other => other,
    })
}

fn num_cmp(a: &Num, b: &Num) -> Option<Ordering> {
    match (a, b) {
        (Num::Int(a), Num::Int(b)) => Some(a.cmp(b)),
        (Num::Float(a), Num::Float(b)) => a.partial_cmp(b),
        (Num::Int(a), Num::Float(b)) => int_float_cmp(a, *b),
        (Num::Float(a), Num::Int(b)) => int_float_cmp(b, *a).map(Ordering::reverse),
    }
}

/// Python `==`.
fn py_eq(a: &Value, b: &Value) -> bool {
    if let (Some(x), Some(y)) = (as_num(a), as_num(b)) {
        return num_cmp(&x, &y) == Some(Ordering::Equal);
    }
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Bytes(x), Value::Bytes(y)) => x == y,
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q))
        }
        (Value::Map(x), Value::Map(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| py_eq(v, w)))
        }
        _ => false,
    }
}

/// Python `type(v).__name__`.
fn py_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Str(_) => "str",
        Value::Bytes(_) => "bytes",
        Value::List(_) => "list",
        Value::Map(_) => "dict",
    }
}

/// Python rich comparison `a <op> b` between numbers and strings; other
/// combinations raise `TypeError` like Python.
fn py_compare(op: &str, a: &Value, b: &Value) -> Result<bool, Error> {
    let ordering = match (as_num(a), as_num(b)) {
        (Some(x), Some(y)) => num_cmp(&x, &y),
        _ => match (a, b) {
            (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
            _ => {
                return Err(Error::new(
                    "TypeError",
                    format!(
                        "'{op}' not supported between instances of '{}' and '{}'",
                        py_type_name(a),
                        py_type_name(b)
                    ),
                ));
            }
        },
    };
    Ok(match ordering {
        None => false,
        Some(o) => match op {
            "<" => o == Ordering::Less,
            "<=" => o != Ordering::Greater,
            ">" => o == Ordering::Greater,
            _ => o != Ordering::Less,
        },
    })
}

/// `list.__getitem__(int)`; `None` for `IndexError`.
fn py_index<'a>(list: &'a [Value], i: &BigInt) -> Option<&'a Value> {
    let i = i.to_i64()?;
    let len = list.len() as i64;
    let i = if i < 0 { i + len } else { i };
    if (0..len).contains(&i) {
        list.get(i as usize)
    } else {
        None
    }
}

/// `list[slice(start, stop, step)]` (`PySlice_Unpack` and
/// `PySlice_AdjustIndices`).
fn py_slice(
    list: &[Value],
    start: Option<&BigInt>,
    stop: Option<&BigInt>,
    step: Option<&BigInt>,
) -> Result<Vec<Value>, Error> {
    let clamp = |v: &BigInt| -> i128 {
        v.to_i64().map(i128::from).unwrap_or(if v.is_negative() {
            i128::from(i64::MIN)
        } else {
            i128::from(i64::MAX)
        })
    };
    let step = match step {
        None => 1,
        Some(s) => {
            if s.is_zero() {
                return Err(Error::new("ValueError", "slice step cannot be zero"));
            }
            clamp(s).max(-i128::from(i64::MAX))
        }
    };
    let len = list.len() as i128;
    let adjust = |v: Option<&BigInt>, default: i128| -> i128 {
        let Some(v) = v else { return default };
        let mut v = clamp(v);
        if v < 0 {
            v += len;
            if v < 0 {
                v = if step < 0 { -1 } else { 0 };
            }
        } else if v >= len {
            v = if step < 0 { len - 1 } else { len };
        }
        v
    };
    let (start, stop) = if step < 0 {
        (adjust(start, len - 1), adjust(stop, -1))
    } else {
        (adjust(start, 0), adjust(stop, len))
    };
    let mut out = Vec::new();
    let mut i = start;
    if step > 0 {
        while i < stop {
            out.push(list[i as usize].clone());
            i += step;
        }
    } else {
        while i > stop {
            out.push(list[i as usize].clone());
            i += step;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Functions (`jmespath/functions.py`)

/// `Functions.FUNCTION_TABLE`: the argument types and whether the last
/// argument is variadic.
fn signature(name: &str) -> Option<(&'static [&'static [&'static str]], bool)> {
    Some(match name {
        "abs" | "ceil" | "floor" => (&[&["number"]], false),
        "avg" | "sum" => (&[&["array-number"]], false),
        "not_null" => (&[&[]], true),
        "to_array" | "to_string" | "to_number" | "type" => (&[&[]], false),
        "contains" => (&[&["array", "string"], &[]], false),
        "length" => (&[&["string", "array", "object"]], false),
        "ends_with" | "starts_with" => (&[&["string"], &["string"]], false),
        "reverse" => (&[&["array", "string"]], false),
        "join" => (&[&["string"], &["array-string"]], false),
        "map" => (&[&["expref"], &["array"]], false),
        "max" | "min" => (&[&["array-number", "array-string"]], false),
        "merge" => (&[&["object"]], true),
        "sort" => (&[&["array-string", "array-number"]], false),
        "keys" | "values" => (&[&["object"]], false),
        "sort_by" | "min_by" | "max_by" => (&[&["array"], &["expref"]], false),
        _ => return None,
    })
}

/// `functions.TYPES_MAP` lookup by Python type name.
fn jmespath_type(arg: &Arg) -> &'static str {
    match arg {
        Arg::Expref(_) => "expref",
        Arg::Value(v) => match v {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Int(_) | Value::Float(_) => "number",
            Value::Str(_) => "string",
            Value::Bytes(_) => "unknown",
            Value::List(_) => "array",
            Value::Map(_) => "object",
        },
    }
}

fn arg_type_name(arg: &Arg) -> &'static str {
    match arg {
        Arg::Expref(_) => "_Expression",
        Arg::Value(v) => py_type_name(v),
    }
}

/// `'%s' % value` for an argument.
fn arg_str(arg: &Arg) -> String {
    match arg {
        Arg::Expref(_) => "<jmespath.visitor._Expression object>".to_string(),
        Arg::Value(v) => pyfmt::py_str(v),
    }
}

/// Python `repr(list_of_str)`.
fn str_list_repr(items: &[&str]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|t| format!("'{t}'"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// `exceptions.JMESPathTypeError.__str__`.
fn type_error(function: &str, current: &str, actual: &str, expected: &[&str]) -> Error {
    Error::new(
        "JMESPathTypeError",
        format!(
            "In function {function}(), invalid type for value: {current}, expected one of: {}, received: \"{actual}\"",
            str_list_repr(expected)
        ),
    )
}

/// `functions.REVERSE_TYPES_MAP`, as the jmespath types it admits.
fn admits(jmes_type: &str, arg: &Arg) -> bool {
    jmespath_type(arg) == jmes_type && !matches!(arg, Arg::Value(Value::Bytes(_)))
}

/// `Functions._type_check_single`.
fn type_check_single(function: &str, current: &Arg, types: &[&str]) -> Result<(), Error> {
    let mut allowed_types = Vec::new();
    let mut allowed_subtypes = Vec::new();
    for t in types {
        match t.split_once('-') {
            Some((ty, sub)) => {
                allowed_types.push(ty);
                allowed_subtypes.push(sub);
            }
            None => allowed_types.push(*t),
        }
    }
    if !allowed_types.iter().any(|t| admits(t, current)) {
        return Err(type_error(
            function,
            &arg_str(current),
            jmespath_type(current),
            types,
        ));
    }
    if allowed_subtypes.is_empty() {
        return Ok(());
    }
    let Arg::Value(Value::List(elements)) = current else {
        return Ok(());
    };
    let element_error = |element: &Value| {
        type_error(
            function,
            &pyfmt::py_str(element),
            py_type_name(element),
            types,
        )
    };
    let fits = |sub: &str, element: &Value| admits(sub, &Arg::Value(element.clone()));
    if allowed_subtypes.len() == 1 {
        for element in elements {
            if !fits(allowed_subtypes[0], element) {
                return Err(element_error(element));
            }
        }
    } else if let Some(first) = elements.first() {
        let Some(allowed) = allowed_subtypes.iter().find(|sub| fits(sub, first)) else {
            return Err(element_error(first));
        };
        for element in elements {
            if !fits(allowed, element) {
                return Err(element_error(element));
            }
        }
    }
    Ok(())
}

fn pluralize(word: &str, count: usize) -> String {
    if count == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// `Functions.call_function`.
fn call_function(name: &str, args: Vec<Arg>) -> Result<Value, Error> {
    let Some((sig, variadic)) = signature(name) else {
        return Err(Error::new(
            "UnknownFunctionError",
            format!("Unknown function: {name}()"),
        ));
    };
    if variadic {
        if args.len() < sig.len() {
            return Err(Error::new(
                "VariadictArityError",
                format!(
                    "Expected at least {} {} for function {name}(), received {}",
                    sig.len(),
                    pluralize("argument", sig.len()),
                    args.len()
                ),
            ));
        }
    } else if args.len() != sig.len() {
        return Err(Error::new(
            "ArityError",
            format!(
                "Expected {} {} for function {name}(), received {}",
                sig.len(),
                pluralize("argument", sig.len()),
                args.len()
            ),
        ));
    }
    for (types, arg) in sig.iter().zip(&args) {
        if !types.is_empty() {
            type_check_single(name, arg, types)?;
        }
    }
    dispatch(name, args)
}

fn value_arg(arg: Arg) -> Result<Value, Error> {
    match arg {
        Arg::Value(v) => Ok(v),
        Arg::Expref(_) => Err(expref_unsupported()),
    }
}

fn dispatch(name: &str, args: Vec<Arg>) -> Result<Value, Error> {
    match name {
        "not_null" => {
            for arg in args {
                match arg {
                    Arg::Value(Value::Null) => {}
                    other => return value_arg(other),
                }
            }
            return Ok(Value::Null);
        }
        "merge" => {
            let mut merged = Map::new();
            for arg in args {
                dict_update(&mut merged, arg)?;
            }
            return Ok(Value::Map(merged));
        }
        _ => {}
    }
    let mut args = args.into_iter();
    let mut next = || args.next().expect("arity checked");
    match name {
        "abs" => Ok(match value_arg(next())? {
            Value::Int(i) => Value::Int(i.abs()),
            Value::Float(f) => Value::Float(f.abs()),
            other => other,
        }),
        "avg" => {
            let Value::List(l) = value_arg(next())? else {
                unreachable!()
            };
            if l.is_empty() {
                return Ok(Value::Null);
            }
            let total = match py_sum(&l)? {
                Value::Int(i) => int_to_f64(&i)?,
                Value::Float(f) => f,
                _ => unreachable!(),
            };
            Ok(Value::Float(total / l.len() as f64))
        }
        "to_array" => Ok(match value_arg(next())? {
            Value::List(l) => Value::List(l),
            other => Value::List(vec![other]),
        }),
        "to_string" => Ok(match value_arg(next())? {
            Value::Str(s) => Value::Str(s),
            other => {
                let mut out = String::new();
                json_dumps_compact(&other, &mut out);
                Value::Str(out)
            }
        }),
        "to_number" => to_number(next()),
        "contains" => {
            let subject = value_arg(next())?;
            let search = next();
            match subject {
                Value::List(l) => {
                    let search = value_arg(search)?;
                    Ok(Value::Bool(l.iter().any(|x| py_eq(x, &search))))
                }
                Value::Str(s) => match search {
                    Arg::Value(Value::Str(needle)) => Ok(Value::Bool(s.contains(&needle))),
                    other => Err(Error::new(
                        "TypeError",
                        format!(
                            "'in <string>' requires string as left operand, not {}",
                            arg_type_name(&other)
                        ),
                    )),
                },
                _ => unreachable!(),
            }
        }
        "length" => Ok(Value::int(match value_arg(next())? {
            Value::Str(s) => s.chars().count() as i64,
            Value::List(l) => l.len() as i64,
            Value::Map(m) => m.len() as i64,
            _ => unreachable!(),
        })),
        "ends_with" | "starts_with" => {
            let (Value::Str(subject), Value::Str(affix)) = (value_arg(next())?, value_arg(next())?)
            else {
                unreachable!()
            };
            Ok(Value::Bool(if name == "ends_with" {
                subject.ends_with(&affix)
            } else {
                subject.starts_with(&affix)
            }))
        }
        "reverse" => Ok(match value_arg(next())? {
            Value::Str(s) => Value::Str(s.chars().rev().collect()),
            Value::List(mut l) => {
                l.reverse();
                Value::List(l)
            }
            _ => unreachable!(),
        }),
        "ceil" | "floor" => match value_arg(next())? {
            Value::Int(i) => Ok(Value::Int(i)),
            Value::Float(f) => {
                if f.is_nan() {
                    return Err(Error::new(
                        "ValueError",
                        "cannot convert float NaN to integer",
                    ));
                }
                if f.is_infinite() {
                    return Err(Error::new(
                        "OverflowError",
                        "cannot convert float infinity to integer",
                    ));
                }
                let r = if name == "ceil" { f.ceil() } else { f.floor() };
                Ok(Value::Int(BigInt::from_f64(r).expect("finite float")))
            }
            _ => unreachable!(),
        },
        "join" => {
            let (Value::Str(sep), Value::List(items)) = (value_arg(next())?, value_arg(next())?)
            else {
                unreachable!()
            };
            let parts: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            Ok(Value::Str(parts.join(&sep)))
        }
        "map" => {
            let Arg::Expref(expr) = next() else {
                unreachable!()
            };
            let Value::List(items) = value_arg(next())? else {
                unreachable!()
            };
            let mut result = Vec::with_capacity(items.len());
            for element in &items {
                result.push(visit(expr, element)?);
            }
            Ok(Value::List(result))
        }
        "max" | "min" => {
            let Value::List(items) = value_arg(next())? else {
                unreachable!()
            };
            let op = if name == "max" { ">" } else { "<" };
            let mut best: Option<&Value> = None;
            for item in &items {
                match best {
                    None => best = Some(item),
                    Some(current) => {
                        if py_compare(op, item, current)? {
                            best = Some(item);
                        }
                    }
                }
            }
            Ok(best.cloned().unwrap_or(Value::Null))
        }
        "sort" => {
            let Value::List(items) = value_arg(next())? else {
                unreachable!()
            };
            Ok(Value::List(stable_sort(items, |a, b| {
                py_compare("<", a, b)
            })?))
        }
        "sum" => {
            let Value::List(items) = value_arg(next())? else {
                unreachable!()
            };
            py_sum(&items)
        }
        "keys" => {
            let Value::Map(m) = value_arg(next())? else {
                unreachable!()
            };
            Ok(Value::List(m.into_keys().map(Value::Str).collect()))
        }
        "values" => {
            let Value::Map(m) = value_arg(next())? else {
                unreachable!()
            };
            Ok(Value::List(m.into_values().collect()))
        }
        "type" => {
            let arg = next();
            Ok(match jmespath_type(&arg) {
                "expref" | "unknown" => Value::Null,
                t => Value::str(t),
            })
        }
        "sort_by" => {
            let Value::List(items) = value_arg(next())? else {
                unreachable!()
            };
            let Arg::Expref(expr) = next() else {
                unreachable!()
            };
            if items.is_empty() {
                return Ok(Value::List(items));
            }
            let first = Arg::Value(visit(expr, &items[0])?);
            let required = jmespath_type(&first);
            if required != "number" && required != "string" {
                return Err(type_error(
                    "sort_by",
                    &pyfmt::py_str(&items[0]),
                    required,
                    &["string", "number"],
                ));
            }
            let mut keyed = Vec::with_capacity(items.len());
            for item in items {
                let key = key_func(expr, &item, &[required], "sort_by")?;
                keyed.push((key, item));
            }
            let sorted = stable_sort(keyed, |a, b| py_compare("<", &a.0, &b.0))?;
            Ok(Value::List(sorted.into_iter().map(|(_, v)| v).collect()))
        }
        "min_by" | "max_by" => {
            let Value::List(items) = value_arg(next())? else {
                unreachable!()
            };
            let Arg::Expref(expr) = next() else {
                unreachable!()
            };
            let op = if name == "max_by" { ">" } else { "<" };
            let mut best: Option<(Value, &Value)> = None;
            for item in &items {
                let key = key_func(expr, item, &["number", "string"], name)?;
                best = match best {
                    None => Some((key, item)),
                    Some((best_key, best_item)) => {
                        if py_compare(op, &key, &best_key)? {
                            Some((key, item))
                        } else {
                            Some((best_key, best_item))
                        }
                    }
                };
            }
            Ok(best.map(|(_, v)| v.clone()).unwrap_or(Value::Null))
        }
        _ => unreachable!("function table"),
    }
}

/// `Functions._create_key_func`.
fn key_func(expr: &Node, item: &Value, allowed: &[&str], function: &str) -> Result<Value, Error> {
    let result = visit(expr, item)?;
    let arg = Arg::Value(result);
    let ty = jmespath_type(&arg);
    if !allowed.contains(&ty) {
        return Err(type_error(function, &arg_str(&arg), ty, allowed));
    }
    value_arg(arg)
}

/// A stable merge sort driven by a fallible `<`, standing in for Python's
/// `sorted` (identical results for a consistent ordering).
fn stable_sort<T>(
    items: Vec<T>,
    mut lt: impl FnMut(&T, &T) -> Result<bool, Error>,
) -> Result<Vec<T>, Error> {
    fn merge_sort<T>(
        mut items: Vec<T>,
        lt: &mut dyn FnMut(&T, &T) -> Result<bool, Error>,
    ) -> Result<Vec<T>, Error> {
        if items.len() <= 1 {
            return Ok(items);
        }
        let right = items.split_off(items.len() / 2);
        let left = merge_sort(items, lt)?;
        let right = merge_sort(right, lt)?;
        let mut out = Vec::with_capacity(left.len() + right.len());
        let mut left = left.into_iter().peekable();
        let mut right = right.into_iter().peekable();
        while let (Some(l), Some(r)) = (left.peek(), right.peek()) {
            if lt(r, l)? {
                out.push(right.next().expect("peeked"));
            } else {
                out.push(left.next().expect("peeked"));
            }
        }
        out.extend(left);
        out.extend(right);
        Ok(out)
    }
    merge_sort(items, &mut lt)
}

/// `float(int)` (`PyLong_AsDouble`).
fn int_to_f64(i: &BigInt) -> Result<f64, Error> {
    match i.to_f64() {
        Some(f) if f.is_finite() => Ok(f),
        _ => Err(Error::new(
            "OverflowError",
            "int too large to convert to float",
        )),
    }
}

/// Python `a + b` for numbers.
fn py_add(a: &Value, b: &Value) -> Result<Value, Error> {
    Ok(match (a, b) {
        (Value::Int(x), Value::Int(y)) => Value::Int(x + y),
        (Value::Int(x), Value::Float(y)) => Value::Float(int_to_f64(x)? + y),
        (Value::Float(x), Value::Int(y)) => Value::Float(x + int_to_f64(y)?),
        (Value::Float(x), Value::Float(y)) => Value::Float(x + y),
        _ => unreachable!("numbers"),
    })
}

/// CPython `builtin_sum_impl` (3.14) over numbers: an `int` fast path,
/// then a compensated (Neumaier) `float` path, then plain addition.
fn py_sum(items: &[Value]) -> Result<Value, Error> {
    let mut rest = items.iter();
    let mut i_result: i64 = 0;
    let mut result = loop {
        let Some(item) = rest.next() else {
            return Ok(Value::int(i_result));
        };
        if let Value::Int(i) = item
            && let Some(sum) = i.to_i64().and_then(|b| i_result.checked_add(b))
        {
            i_result = sum;
            continue;
        }
        break py_add(&Value::int(i_result), item)?;
    };
    if let Value::Float(f) = result {
        let (mut hi, mut lo) = (f, 0.0f64);
        let finish = |hi: f64, lo: f64| {
            if lo != 0.0 && lo.is_finite() {
                hi + lo
            } else {
                hi
            }
        };
        loop {
            let Some(item) = rest.next() else {
                return Ok(Value::Float(finish(hi, lo)));
            };
            let x = match item {
                Value::Float(x) => Some(*x),
                Value::Int(i) => int_to_f64(i).ok(),
                _ => None,
            };
            if let Some(x) = x {
                let t = hi + x;
                if hi.abs() >= x.abs() {
                    lo += (hi - t) + x;
                } else {
                    lo += (x - t) + hi;
                }
                hi = t;
                continue;
            }
            result = py_add(&Value::Float(finish(hi, lo)), item)?;
            break;
        }
    }
    for item in rest {
        result = py_add(&result, item)?;
    }
    Ok(result)
}

/// `Functions._func_to_number`: `int(arg)`, else `float(arg)`, else
/// `None`.
fn to_number(arg: Arg) -> Result<Value, Error> {
    let text = match arg {
        Arg::Expref(_) => {
            return Err(Error::new(
                "TypeError",
                "int() argument must be a string, a bytes-like object or a real number, not '_Expression'",
            ));
        }
        Arg::Value(v) => match v {
            Value::List(_) | Value::Map(_) | Value::Bool(_) | Value::Null => {
                return Ok(Value::Null);
            }
            Value::Int(_) | Value::Float(_) => return Ok(v),
            Value::Str(s) => s,
            Value::Bytes(b) => String::from_utf8_lossy(&b).into_owned(),
        },
    };
    if let Some(i) = py_int_parse(&text) {
        return Ok(Value::Int(i));
    }
    Ok(py_float_parse(&text).map_or(Value::Null, Value::Float))
}

/// Remove `_` separators that sit between two ASCII digits; `None` if any
/// other `_` appears (CPython `_Py_string_to_number_with_underscores`).
fn strip_digit_underscores(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, c) in chars.iter().enumerate() {
        if *c == '_' {
            let prev = i > 0 && chars[i - 1].is_ascii_digit();
            let next = chars.get(i + 1).is_some_and(char::is_ascii_digit);
            if !(prev && next) {
                return None;
            }
        } else {
            out.push(*c);
        }
    }
    Some(out)
}

/// Python `int(str)` in base 10.
fn py_int_parse(s: &str) -> Option<BigInt> {
    let s = py_strip(s);
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if digits.is_empty() || digits.starts_with('_') {
        return None;
    }
    let digits = strip_digit_underscores(digits)?;
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let i: BigInt = digits.parse().ok()?;
    Some(if neg { -i } else { i })
}

/// Python `float(str)`.
fn py_float_parse(s: &str) -> Option<f64> {
    let s = py_strip(s);
    let (sign, body) = match s.strip_prefix('-') {
        Some(b) => (-1.0, b),
        None => (1.0, s.strip_prefix('+').unwrap_or(s)),
    };
    let lower = body.to_ascii_lowercase();
    if lower == "inf" || lower == "infinity" {
        return Some(sign * f64::INFINITY);
    }
    if lower == "nan" {
        return Some(f64::NAN);
    }
    let body = strip_digit_underscores(body)?;
    let bytes = body.as_bytes();
    let mut i = 0;
    let int_digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    i += int_digits;
    let mut frac_digits = 0;
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        frac_digits = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
        i += frac_digits;
    }
    if int_digits + frac_digits == 0 {
        return None;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp_digits = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
        if exp_digits == 0 {
            return None;
        }
        i += exp_digits;
    }
    if i != bytes.len() {
        return None;
    }
    let f: f64 = body.parse().ok()?;
    Some(sign * f)
}

/// `dict.update(arg)` as used by `_func_merge` (only the first argument is
/// type-checked, so later ones follow Python's `update` rules).
fn dict_update(merged: &mut Map, arg: Arg) -> Result<(), Error> {
    let not_iterable =
        |name: &str| Error::new("TypeError", format!("'{name}' object is not iterable"));
    let not_sequence = |n: usize| {
        Error::new(
            "TypeError",
            format!("cannot convert dictionary update sequence element #{n} to a sequence"),
        )
    };
    let bad_length = |n: usize, len: usize| {
        Error::new(
            "ValueError",
            format!("dictionary update sequence element #{n} has length {len}; 2 is required"),
        )
    };
    let value = match arg {
        Arg::Expref(_) => return Err(not_iterable("_Expression")),
        Arg::Value(v) => v,
    };
    match value {
        Value::Map(m) => {
            for (k, v) in m {
                merged.insert(k, v);
            }
            Ok(())
        }
        Value::List(items) => {
            for (n, item) in items.into_iter().enumerate() {
                match item {
                    Value::List(pair) if pair.len() == 2 => {
                        let mut pair = pair.into_iter();
                        let key = pair.next().expect("pair");
                        let value = pair.next().expect("pair");
                        let key = match key {
                            Value::Str(s) => s,
                            Value::List(_) | Value::Map(_) => {
                                return Err(Error::new(
                                    "TypeError",
                                    format!("unhashable type: '{}'", py_type_name(&key)),
                                ));
                            }
                            other => {
                                let mut out = String::new();
                                json_dumps_compact(&other, &mut out);
                                out.trim_matches('"').to_string()
                            }
                        };
                        merged.insert(key, value);
                    }
                    Value::List(pair) => return Err(bad_length(n, pair.len())),
                    Value::Str(s) => {
                        let chars: Vec<char> = s.chars().collect();
                        if chars.len() != 2 {
                            return Err(bad_length(n, chars.len()));
                        }
                        merged.insert(chars[0].to_string(), Value::Str(chars[1].to_string()));
                    }
                    Value::Map(m) if m.len() != 2 => return Err(bad_length(n, m.len())),
                    Value::Map(m) => {
                        let mut keys = m.into_keys();
                        let key = keys.next().expect("two keys");
                        let value = keys.next().expect("two keys");
                        merged.insert(key, Value::Str(value));
                    }
                    Value::Bytes(b) if b.len() != 2 => return Err(bad_length(n, b.len())),
                    _ => return Err(not_sequence(n)),
                }
            }
            Ok(())
        }
        Value::Str(s) if s.is_empty() => Ok(()),
        Value::Str(_) => Err(bad_length(0, 1)),
        Value::Bytes(b) if b.is_empty() => Ok(()),
        Value::Bytes(_) => Err(not_sequence(0)),
        other => Err(not_iterable(py_type_name(&other))),
    }
}

/// `json.dumps(v, separators=(',', ':'), default=str)` (`ensure_ascii`).
fn json_dumps_compact(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) => {
            if f.is_nan() {
                out.push_str("NaN");
            } else if f.is_infinite() {
                out.push_str(if *f > 0.0 { "Infinity" } else { "-Infinity" });
            } else {
                out.push_str(&pyfmt::float_repr(*f));
            }
        }
        Value::Str(s) => json_string_ascii(s, out),
        Value::Bytes(b) => json_string_ascii(&pyfmt::bytes_repr(b), out),
        Value::List(l) => {
            out.push('[');
            for (i, item) in l.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_dumps_compact(item, out);
            }
            out.push(']');
        }
        Value::Map(m) => {
            out.push('{');
            for (i, (k, item)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_string_ascii(k, out);
                out.push(':');
                json_dumps_compact(item, out);
            }
            out.push('}');
        }
    }
}

/// `json.encoder.py_encode_basestring_ascii`.
fn json_string_ascii(s: &str, out: &mut String) {
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
            ' '..='~' => out.push(c),
            _ => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str, data: &str) -> Value {
        search(expr, &Value::parse_json(data).unwrap()).unwrap()
    }

    #[test]
    fn query_keeps_number_types() {
        let r = eval("a[].b", r#"{"a": [{"b": 1}, {"b": 2.5}]}"#);
        assert_eq!(r, Value::List(vec![Value::int(1), Value::Float(2.5)]));
    }

    #[test]
    fn multiselect_hash_keeps_expression_order() {
        let r = eval("{z: a, a: z}", r#"{"a": 1, "z": 2}"#);
        let keys: Vec<&String> = r.as_map().unwrap().keys().collect();
        assert_eq!(keys, ["z", "a"]);
    }

    #[test]
    fn sum_and_avg_follow_python() {
        assert_eq!(eval("sum(@)", "[1, 2]"), Value::int(3));
        assert_eq!(eval("sum(@)", "[0.1, 0.2, 0.3]"), Value::Float(0.6));
        assert_eq!(eval("avg(@)", "[1, 2]"), Value::Float(1.5));
    }

    #[test]
    fn parse_error_message() {
        let e = Expression::compile("foo[").err().unwrap();
        assert_eq!(
            e.to_string(),
            "Invalid jmespath expression: Incomplete expression:\n\"foo[\"\n     ^"
        );
    }
}
