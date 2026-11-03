//! The shorthand syntax (`awscli/shorthand.py`): `ShorthandParser` and
//! `BackCompatVisitor`.

use std::sync::OnceLock;

use regex::Regex;

use super::paramfile::{self, ParamfileError};
use super::pynum::{is_py_space, py_float, py_int};
use crate::model::Shape;
use crate::value::{Map, Value};

/// Errors of the shorthand parser.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ShorthandError {
    /// `ShorthandParseError` (and subclasses): the message.
    Parse(String),
    /// `DocumentTypesNotSupportedError` before the enclosing structure
    /// turns it into a parse error.
    DocumentType,
    /// A paramfile failed to load (`ResourceLoadingError`).
    Paramfile(ParamfileError),
    /// A Python exception other than the parse errors (`ValueError`,
    /// `TypeError`, `AttributeError`) with its `str()`.
    Python(String),
}

/// `string.whitespace`.
fn is_ascii_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}')
}

/// `_START_WORD`.
fn start_word(c: char) -> bool {
    matches!(c,
        '!' | '#'..='&' | '('..='+' | '-'..='<' | '>'..='Z' | '\\'..='z' | '|'..='\u{ffff}')
}

/// `_FIRST_FOLLOW_CHARS`.
fn first_follow(c: char) -> bool {
    is_py_space(c)
        || matches!(c,
            '!' | '#'..='&' | '('..='+' | '-'..='\\' | '^'..='|' | '~'..='\u{ffff}')
}

/// `_SECOND_FOLLOW_CHARS`.
fn second_follow(c: char) -> bool {
    is_py_space(c) || matches!(c, '!' | '#'..='&' | '('..='+' | '-'..='<' | '>'..='\u{ffff}')
}

fn single_quoted() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^'(?:\\'|[^'])*'").expect("regex"))
}

fn double_quoted() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"^"(?:\\"|[^"])*""#).expect("regex"))
}

/// `ShorthandParser`.
pub(crate) struct ShorthandParser {
    input: Vec<char>,
    index: isize,
    resolve_paramfiles: bool,
}

type R<T> = Result<T, ShorthandError>;

impl ShorthandParser {
    /// `ShorthandParser().parse(value)`.
    pub(crate) fn parse(value: &str) -> R<Value> {
        let mut p = ShorthandParser {
            input: value.chars().collect(),
            index: 0,
            resolve_paramfiles: false,
        };
        p.parameter()
    }

    fn idx(&self) -> usize {
        self.index.max(0) as usize
    }

    /// `ShorthandParseError._error_location`.
    fn error_location(&self, index: usize) -> String {
        let value = &self.input;
        let mut consumed: String = value.iter().collect();
        let mut remaining = String::new();
        let mut num_spaces = index;
        let head = &value[..index.min(value.len())];
        if let Some(nl) = head.iter().rposition(|&c| c == '\n') {
            num_spaces = index - nl - 1;
        }
        let tail = &value[index.min(value.len())..];
        if let Some(off) = tail.iter().position(|&c| c == '\n') {
            let next = index + off;
            consumed = value[..next].iter().collect();
            remaining = value[next..].iter().collect();
        }
        format!("{consumed}\n{}^{remaining}", " ".repeat(num_spaces))
    }

    /// `ShorthandParseSyntaxError`.
    fn syntax_error(&self, expected: &str, actual: &str, index: usize) -> ShorthandError {
        ShorthandError::Parse(format!(
            "Expected: '{expected}', received: '{actual}' for input:\n {}",
            self.error_location(index)
        ))
    }

    fn at_eof(&self) -> bool {
        self.idx() >= self.input.len()
    }

    fn current(&self) -> Option<char> {
        self.input.get(self.idx()).copied()
    }

    fn consume_whitespace(&mut self) {
        while self.current().is_some_and(is_ascii_ws) {
            self.index += 1;
        }
    }

    fn expect(&mut self, ch: char, consume_whitespace: bool) -> R<()> {
        if consume_whitespace {
            self.consume_whitespace();
        }
        let Some(actual) = self.current() else {
            return Err(self.syntax_error(&ch.to_string(), "EOF", self.idx()));
        };
        if actual != ch {
            return Err(self.syntax_error(&ch.to_string(), &actual.to_string(), self.idx()));
        }
        self.index += 1;
        if consume_whitespace {
            self.consume_whitespace();
        }
        Ok(())
    }

    /// `_parameter`.
    fn parameter(&mut self) -> R<Value> {
        let mut params = Map::new();
        let (key, val) = self.keyval()?;
        params.insert(key, val);
        let mut last_index = self.idx();
        while self.idx() < self.input.len() {
            self.expect(',', true)?;
            let (key, val) = self.keyval()?;
            if params.contains_key(&key) {
                return Err(ShorthandError::Parse(format!(
                    "Second instance of key \"{key}\" encountered for input:\n{}\nThis is often because there is a preceding \",\" instead of a space.",
                    self.error_location(last_index + 1)
                )));
            }
            params.insert(key, val);
            last_index = self.idx();
        }
        Ok(Value::Map(params))
    }

    /// `_keyval`.
    fn keyval(&mut self) -> R<(String, Value)> {
        let key = self.key();
        self.at_sign_python()?;
        self.expect('=', true)?;
        let values = self.values()?;
        Ok((key, values))
    }

    /// `_expect('@', consume_whitespace=True)` inside `try/except`: the
    /// leading whitespace it consumed stays consumed.
    fn at_sign_python(&mut self) -> R<()> {
        self.resolve_paramfiles = false;
        self.consume_whitespace();
        if self.current() == Some('@') {
            self.index += 1;
            self.consume_whitespace();
            self.resolve_paramfiles = true;
        }
        Ok(())
    }

    /// `_key`.
    fn key(&mut self) -> String {
        let start = self.idx();
        while let Some(c) = self.current() {
            if !(c.is_ascii_alphanumeric() || "-_.#/:".contains(c)) {
                break;
            }
            self.index += 1;
        }
        self.input[start..self.idx()].iter().collect()
    }

    /// `_values`.
    fn values(&mut self) -> R<Value> {
        match self.current() {
            None => Ok(Value::str("")),
            Some('[') => self.explicit_list(),
            Some('{') => self.hash_literal(),
            Some(_) => self.csv_value(),
        }
    }

    /// `_csv_value`.
    fn csv_value(&mut self) -> R<Value> {
        let first = self.first_value()?;
        self.consume_whitespace();
        if self.at_eof() || self.current() != Some(',') {
            return Ok(first);
        }
        self.expect(',', true)?;
        let mut list = vec![first.clone()];
        loop {
            match self.second_value() {
                Ok(current) => {
                    self.consume_whitespace();
                    if self.at_eof() {
                        list.push(current);
                        break;
                    }
                    match self.expect(',', true) {
                        Ok(()) => list.push(current),
                        Err(ShorthandError::Parse(e)) => {
                            if self.at_eof() {
                                return Err(ShorthandError::Parse(e));
                            }
                            self.backtrack_to(',');
                            break;
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(ShorthandError::Parse(e)) => {
                    if self.at_eof() {
                        return Err(ShorthandError::Parse(e));
                    }
                    self.backtrack_to(',');
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        if list.len() == 1 {
            return Ok(first);
        }
        Ok(Value::List(list))
    }

    fn backtrack_to(&mut self, ch: char) {
        while self.index >= 0 && self.input.get(self.index as usize) != Some(&ch) {
            self.index -= 1;
        }
    }

    /// Match `((\\,)|[start])((\\,)|[follow])*` at the current index.
    fn match_word(&self, follow: fn(char) -> bool) -> Option<usize> {
        let s = &self.input[self.idx()..];
        let step = |i: usize, pred: &dyn Fn(char) -> bool| -> Option<usize> {
            if s.get(i) == Some(&'\\') && s.get(i + 1) == Some(&',') {
                Some(2)
            } else if s.get(i).is_some_and(|&c| pred(c)) {
                Some(1)
            } else {
                None
            }
        };
        let mut i = step(0, &start_word)?;
        while let Some(n) = step(i, &follow) {
            i += n;
        }
        Some(i)
    }

    fn take(&mut self, n: usize) -> String {
        let start = self.idx();
        self.index += n as isize;
        self.input[start..start + n].iter().collect()
    }

    fn maybe_paramfile(&self, processed: String) -> R<Value> {
        if self.resolve_paramfiles {
            match paramfile::get_paramfile(&processed) {
                Ok(Some(v)) => return Ok(v),
                Ok(None) => {}
                Err(e) => return Err(ShorthandError::Paramfile(e)),
            }
        }
        Ok(Value::Str(processed))
    }

    /// `_value`.
    fn value(&mut self) -> R<Value> {
        match self.match_word(first_follow) {
            Some(n) => {
                let consumed = self.take(n);
                let processed = consumed.replace("\\,", ",");
                let processed = processed.trim_end_matches(is_py_space).to_string();
                self.maybe_paramfile(processed)
            }
            None => Ok(Value::str("")),
        }
    }

    /// `_explicit_list`.
    fn explicit_list(&mut self) -> R<Value> {
        self.expect('[', true)?;
        let mut values = Vec::new();
        while self.current() != Some(']') {
            values.push(self.explicit_values()?);
            self.consume_whitespace();
            if self.current() != Some(']') {
                self.expect(',', false)?;
                self.consume_whitespace();
            }
        }
        self.expect(']', false)?;
        Ok(Value::List(values))
    }

    /// `_explicit_values`.
    fn explicit_values(&mut self) -> R<Value> {
        match self.current() {
            Some('[') => self.explicit_list(),
            Some('{') => self.hash_literal(),
            _ => self.first_value(),
        }
    }

    /// `_hash_literal`.
    fn hash_literal(&mut self) -> R<Value> {
        self.expect('{', true)?;
        let mut keyvals = Map::new();
        while self.current() != Some('}') {
            let key = self.key();
            self.at_sign_python()?;
            self.expect('=', true)?;
            let v = self.explicit_values()?;
            self.consume_whitespace();
            if self.current() != Some('}') {
                self.expect(',', false)?;
                self.consume_whitespace();
            }
            keyvals.insert(key, v);
        }
        self.expect('}', false)?;
        Ok(Value::Map(keyvals))
    }

    /// `_first_value`.
    fn first_value(&mut self) -> R<Value> {
        match self.current() {
            Some('\'') => self.quoted(single_quoted(), '\''),
            Some('"') => self.quoted(double_quoted(), '"'),
            _ => self.value(),
        }
    }

    /// `_single_quoted_value` / `_double_quoted_value`.
    fn quoted(&mut self, re: &Regex, quote: char) -> R<Value> {
        let rest: String = self.input[self.idx()..].iter().collect();
        let Some(m) = re.find(&rest) else {
            let name = if quote == '\'' {
                "singled quoted"
            } else {
                "double quoted"
            };
            return Err(self.syntax_error(&format!("<{name}>"), "<none>", self.idx()));
        };
        let n = m.as_str().chars().count();
        let consumed = self.take(n);
        let inner: String = {
            let chars: Vec<char> = consumed.chars().collect();
            chars[1..chars.len() - 1].iter().collect()
        };
        let processed = inner
            .replace(&format!("\\{quote}"), &quote.to_string())
            .replace("\\\\", "\\");
        self.maybe_paramfile(processed)
    }

    /// `_second_value`.
    fn second_value(&mut self) -> R<Value> {
        match self.current() {
            Some('\'') => self.quoted(single_quoted(), '\''),
            Some('"') => self.quoted(double_quoted(), '"'),
            _ => {
                let Some(n) = self.match_word(second_follow) else {
                    return Err(self.syntax_error("<second>", "<none>", self.idx()));
                };
                let consumed = self.take(n);
                let processed = consumed.replace("\\,", ",");
                let processed = processed.trim_end_matches(is_py_space).to_string();
                self.maybe_paramfile(processed)
            }
        }
    }
}

/// Python's type name for error messages.
pub(crate) fn py_type_name(v: &Value) -> &'static str {
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

/// Python `int(value)` on a parsed value.
pub(crate) fn int_of(v: &Value) -> Result<Value, String> {
    match v {
        Value::Int(_) => Ok(v.clone()),
        Value::Bool(b) => Ok(Value::int(i64::from(*b))),
        Value::Float(f) => {
            if f.is_nan() {
                Err("cannot convert float NaN to integer".to_string())
            } else if f.is_infinite() {
                Err("cannot convert float infinity to integer".to_string())
            } else {
                let i: num_bigint::BigInt =
                    num_traits::FromPrimitive::from_f64(f.trunc()).expect("finite");
                Ok(Value::Int(i))
            }
        }
        Value::Str(s) => py_int(s).map(Value::Int).ok_or_else(|| {
            format!(
                "invalid literal for int() with base 10: {}",
                crate::pyfmt::str_repr(s)
            )
        }),
        Value::Bytes(b) => {
            let s = String::from_utf8_lossy(b);
            py_int(&s).map(Value::Int).ok_or_else(|| {
                format!(
                    "invalid literal for int() with base 10: {}",
                    crate::pyfmt::bytes_repr(b)
                )
            })
        }
        other => Err(format!(
            "int() argument must be a string, a bytes-like object or a real number, not '{}'",
            py_type_name(other)
        )),
    }
}

/// Python `float(value)` on a parsed value.
pub(crate) fn float_of(v: &Value) -> Result<Value, String> {
    match v {
        Value::Float(_) => Ok(v.clone()),
        Value::Bool(b) => Ok(Value::Float(if *b { 1.0 } else { 0.0 })),
        Value::Int(i) => num_traits::ToPrimitive::to_f64(i)
            .filter(|f| f.is_finite())
            .map(Value::Float)
            .ok_or_else(|| "int too large to convert to float".to_string()),
        Value::Str(s) => py_float(s).map(Value::Float).ok_or_else(|| {
            format!(
                "could not convert string to float: {}",
                crate::pyfmt::str_repr(s)
            )
        }),
        Value::Bytes(b) => py_float(&String::from_utf8_lossy(b))
            .map(Value::Float)
            .ok_or_else(|| {
                format!(
                    "could not convert string to float: {}",
                    crate::pyfmt::bytes_repr(b)
                )
            }),
        other => Err(format!(
            "float() argument must be a string or a real number, not '{}'",
            py_type_name(other)
        )),
    }
}

/// `BackCompatVisitor().visit(params, model)`.
pub(crate) fn back_compat_visit(params: &mut Value, model: &Shape<'_>) -> R<()> {
    visit(params, model, true)
}

/// `ModelVisitor._visit` with the `BackCompatVisitor` overrides. `root`
/// marks the value whose parent is the throwaway `{}` of `visit()`, so
/// replacements of it are dropped.
fn visit(value: &mut Value, shape: &Shape<'_>, root: bool) -> R<()> {
    match shape.type_name() {
        "structure" => {
            if !matches!(value, Value::Null) && shape.is_document() {
                return Err(ShorthandError::DocumentType);
            }
            let Value::Map(m) = value else { return Ok(()) };
            for (member, member_shape) in shape.members() {
                let mut absent = Value::Null;
                let slot = match m.get_mut(member) {
                    Some(v) => v,
                    None => &mut absent,
                };
                match visit(slot, &member_shape, false) {
                    Ok(()) => {}
                    Err(ShorthandError::DocumentType) => {
                        return Err(ShorthandError::Parse(format!(
                            "Shorthand syntax does not support document types. Use JSON input for top-level argument to specify nested parameter: {member}"
                        )));
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
        "list" => {
            if let Value::List(items) = value {
                let Some(member) = shape.list_member() else {
                    return Ok(());
                };
                for item in items {
                    visit(item, &member, false)?;
                }
            } else if !matches!(value, Value::Null) && !root {
                let old = std::mem::take(value);
                *value = Value::List(vec![old]);
            }
            Ok(())
        }
        "map" => {
            if let Value::Map(m) = value {
                let Some(vshape) = shape.map_value() else {
                    return Ok(());
                };
                for (_, v) in m.iter_mut() {
                    visit(v, &vshape, false)?;
                }
            }
            Ok(())
        }
        t => {
            if matches!(value, Value::Null) {
                return Ok(());
            }
            let converted = match t {
                "integer" | "long" => Some(int_of(value).map_err(ShorthandError::Python)?),
                "double" | "float" => Some(float_of(value).map_err(ShorthandError::Python)?),
                "boolean" => match value {
                    Value::Str(s) if s.to_lowercase() == "true" => Some(Value::Bool(true)),
                    Value::Str(s) if s.to_lowercase() == "false" => Some(Value::Bool(false)),
                    Value::Str(_) => None,
                    other => {
                        return Err(ShorthandError::Python(format!(
                            "'{}' object has no attribute 'lower'",
                            py_type_name(other)
                        )));
                    }
                },
                _ => None,
            };
            if let Some(c) = converted
                && !root
            {
                *value = c;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
