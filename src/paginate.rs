//! Pagination (`botocore/paginate.py`, `awscli/customizations/paginate.py`).

use anyhow::Result;
use base64::Engine;
use num_bigint::BigInt;
use num_traits::ToPrimitive;

use crate::bjson::{Node, Obj};
use crate::jmes;
use crate::model::Operation;
use crate::pyfmt;
use crate::value::{Map, Value};

/// User-facing pagination options (`--starting-token`, `--max-items`,
/// `--page-size`).
#[derive(Debug, Clone, Default)]
pub struct PaginationConfig {
    pub starting_token: Option<String>,
    pub max_items: Option<i64>,
    pub page_size: Option<i64>,
}

/// The fully consumed result of a `PageIterator`, carrying what the
/// formatters read from it.
#[derive(Debug, Clone)]
pub struct PaginatedResponse {
    /// Each page as parsed (including `ResponseMetadata`), truncated the way
    /// `PageIterator` truncates when `MaxItems` is reached.
    pub pages: Vec<Value>,
    /// The `result_key` JMESPath expressions.
    pub result_keys: Vec<String>,
    /// `PageIterator.non_aggregate_part`.
    pub non_aggregate_part: Map,
    /// `PageIterator.resume_token` (already encoded).
    pub resume_token: Option<String>,
    /// `PageIterator.build_full_result()`.
    pub full_result: Value,
}

/// The exceptions pagination raises, with `Display` equal to Python's
/// `str(exception)`. The AWS CLI reports all of them through
/// `GeneralExceptionHandler` (exit status 255).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PaginateError {
    /// botocore `PaginationError`.
    #[error("Error during pagination: {0}")]
    Pagination(String),
    /// `ValueError` (`Bad starting token: ...`).
    #[error("{0}")]
    Value(String),
    /// Any other Python exception (`TypeError`, `KeyError`, ...).
    #[error("{0}")]
    Internal(String),
}

impl PaginateError {
    pub fn exit_code(&self) -> i32 {
        255
    }
}

/// Issues one API call with the given parameters and returns the parsed
/// response.
pub type CallFn<'a> = dyn FnMut(&Value) -> Result<Value> + 'a;

/// A compiled JMESPath expression. Plain dotted field paths (every
/// `result_key` and `non_aggregate_keys` entry in the models) are walked
/// directly, which is what JMESPath does for them; anything else goes to
/// [`crate::jmes`].
pub(crate) struct Expr {
    source: String,
    fields: Option<Vec<String>>,
    compiled: Option<jmes::Expression>,
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Expr {
    pub(crate) fn compile(source: &str) -> Result<Expr> {
        let parts: Vec<&str> = source.split('.').collect();
        if parts.iter().all(|p| is_identifier(p)) {
            return Ok(Expr {
                source: source.to_string(),
                fields: Some(parts.into_iter().map(str::to_string).collect()),
                compiled: None,
            });
        }
        Ok(Expr {
            source: source.to_string(),
            fields: None,
            compiled: Some(jmes::Expression::compile(source)?),
        })
    }

    pub(crate) fn search(&self, data: &Value) -> Result<Value> {
        match &self.fields {
            Some(fields) => {
                let mut current = data;
                for f in fields {
                    match current.get(f) {
                        Some(v) => current = v,
                        None => return Ok(Value::Null),
                    }
                }
                Ok(current.clone())
            }
            None => self.compiled.as_ref().unwrap().search(data),
        }
    }
}

fn type_name(v: &Value) -> &'static str {
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

fn internal(msg: impl Into<String>) -> anyhow::Error {
    PaginateError::Internal(msg.into()).into()
}

/// Python `len(value)`.
fn py_len(v: &Value) -> Result<i64> {
    match v {
        Value::Str(s) => Ok(s.chars().count() as i64),
        Value::Bytes(b) => Ok(b.len() as i64),
        Value::List(l) => Ok(l.len() as i64),
        Value::Map(m) => Ok(m.len() as i64),
        other => Err(internal(format!(
            "object of type '{}' has no len()",
            type_name(other)
        ))),
    }
}

/// The bounds of Python's `seq[start:stop]` for a sequence of length `len`.
fn slice_bounds(len: i64, start: Option<i64>, stop: Option<i64>) -> (usize, usize) {
    let norm = |i: i64| {
        if i < 0 { (len + i).max(0) } else { i.min(len) }
    };
    let a = start.map(norm).unwrap_or(0);
    let b = stop.map(norm).unwrap_or(len);
    (a as usize, b.max(a) as usize)
}

/// Python `seq[start:stop]` for `list` and `str`.
fn py_slice(v: &Value, start: Option<i64>, stop: Option<i64>) -> Result<Value> {
    match v {
        Value::List(l) => {
            let (a, b) = slice_bounds(l.len() as i64, start, stop);
            Ok(Value::List(l[a..b].to_vec()))
        }
        Value::Str(s) => {
            let chars: Vec<char> = s.chars().collect();
            let (a, b) = slice_bounds(chars.len() as i64, start, stop);
            Ok(Value::Str(chars[a..b].iter().collect()))
        }
        other => Err(internal(format!(
            "'{}' object is not subscriptable",
            type_name(other)
        ))),
    }
}

/// A slice index as Python accepts it (`int`, `bool`, or `None`).
fn slice_index(v: &Value) -> Result<Option<i64>> {
    match v {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b as i64)),
        Value::Int(i) => Ok(Some(i.to_i64().unwrap_or(
            if i.sign() == num_bigint::Sign::Minus {
                i64::MIN
            } else {
                i64::MAX
            },
        ))),
        _ => Err(internal(
            "slice indices must be integers or None or have an __index__ method",
        )),
    }
}

/// Python `a + b` for the `int` / `float` / `str` values
/// `build_full_result` and `_truncate_response` add.
fn py_add(a: &Value, b: &Value) -> Result<Value> {
    let as_int = |v: &Value| match v {
        Value::Int(i) => Some(i.clone()),
        Value::Bool(b) => Some(BigInt::from(*b as i64)),
        _ => None,
    };
    let as_float = |v: &Value| match v {
        Value::Float(f) => Some(*f),
        other => as_int(other).and_then(|i| i.to_f64()),
    };
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => Ok(Value::Str(format!("{x}{y}"))),
        (Value::Str(_), other) => Err(internal(format!(
            "can only concatenate str (not \"{}\") to str",
            type_name(other)
        ))),
        _ => {
            if let (Some(x), Some(y)) = (as_int(a), as_int(b)) {
                return Ok(Value::Int(x + y));
            }
            match (as_float(a), as_float(b)) {
                (Some(x), Some(y))
                    if matches!(a, Value::Float(_)) || matches!(b, Value::Float(_)) =>
                {
                    Ok(Value::Float(x + y))
                }
                _ => Err(internal(format!(
                    "unsupported operand type(s) for +: '{}' and '{}'",
                    type_name(a),
                    type_name(b)
                ))),
            }
        }
    }
}

/// Python `==` between two values (numbers compare across `bool` /
/// `int` / `float`).
pub(crate) fn py_eq(a: &Value, b: &Value) -> bool {
    let num = |v: &Value| match v {
        Value::Bool(b) => Some((Some(BigInt::from(*b as i64)), *b as i64 as f64)),
        Value::Int(i) => Some((Some(i.clone()), i.to_f64().unwrap_or(f64::NAN))),
        Value::Float(f) => Some((None, *f)),
        _ => None,
    };
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
        _ => match (num(a), num(b)) {
            (Some((Some(x), _)), Some((Some(y), _))) => x == y,
            (Some((_, x)), Some((_, y))) => x == y,
            _ => false,
        },
    }
}

/// `TokenEncoder.encode`: `json.dumps` the token (after base64-encoding
/// any bytes and recording their paths in `boto_encoded_keys`), then
/// base64-encode the JSON.
pub fn encode_token(token: &Map) -> String {
    fn has_bytes(v: &Value) -> bool {
        match v {
            Value::Bytes(_) => true,
            Value::List(l) => l.iter().any(has_bytes),
            Value::Map(m) => m.values().any(has_bytes),
            _ => false,
        }
    }
    fn encode(v: &Value, path: &[Value], encoded: &mut Vec<Value>) -> Value {
        match v {
            Value::Map(m) => Value::Map(
                m.iter()
                    .map(|(k, v)| {
                        let mut p = path.to_vec();
                        p.push(Value::str(k.clone()));
                        (k.clone(), encode(v, &p, encoded))
                    })
                    .collect(),
            ),
            Value::List(l) => Value::List(
                l.iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let mut p = path.to_vec();
                        p.push(Value::int(i as i64));
                        encode(v, &p, encoded)
                    })
                    .collect(),
            ),
            Value::Bytes(b) => {
                encoded.push(Value::List(path.to_vec()));
                Value::Str(base64::engine::general_purpose::STANDARD.encode(b))
            }
            other => other.clone(),
        }
    }
    let token = Value::Map(token.clone());
    let json = if has_bytes(&token) {
        let mut keys = Vec::new();
        let mut encoded = encode(&token, &[], &mut keys);
        if let Value::Map(m) = &mut encoded {
            m.insert("boto_encoded_keys".into(), Value::List(keys));
        }
        pyfmt::json_dumps_default(&encoded)
    } else {
        pyfmt::json_dumps_default(&token)
    };
    base64::engine::general_purpose::STANDARD.encode(json.as_bytes())
}

/// How a token failed to decode: `ValueError` / `TypeError` make
/// `_parse_starting_token` fall back to the legacy format; other
/// exceptions propagate.
#[derive(Debug, Clone, PartialEq)]
enum DecodeError {
    Fallback,
    Fatal(String),
}

/// `binascii.a2b_base64(data, strict_mode=False)` as CPython 3.14 does it:
/// characters outside the alphabet are skipped, `=` only counts as
/// padding after two characters of a quantum, and decoding continues past
/// complete padding.
fn a2b_base64(data: &[u8]) -> Option<Vec<u8>> {
    fn table(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let (mut quad_pos, mut pads, mut leftchar) = (0u8, 0u8, 0u8);
    for &c in data {
        if c == b'=' {
            if quad_pos >= 2 {
                pads = pads.saturating_add(1);
            }
            continue;
        }
        let Some(v) = table(c) else {
            continue;
        };
        pads = 0;
        match quad_pos {
            0 => {
                quad_pos = 1;
                leftchar = v;
            }
            1 => {
                quad_pos = 2;
                out.push((leftchar << 2) | (v >> 4));
                leftchar = v & 0x0f;
            }
            2 => {
                quad_pos = 3;
                out.push((leftchar << 4) | (v >> 2));
                leftchar = v & 0x03;
            }
            _ => {
                quad_pos = 0;
                out.push((leftchar << 6) | v);
                leftchar = 0;
            }
        }
    }
    if quad_pos == 1 || (quad_pos != 0 && quad_pos + pads < 4) {
        return None;
    }
    Some(out)
}

/// `TokenDecoder.decode`.
fn decode_token(token: &str) -> Result<Value, DecodeError> {
    let bytes = a2b_base64(token.as_bytes()).ok_or(DecodeError::Fallback)?;
    let json = String::from_utf8(bytes).map_err(|_| DecodeError::Fallback)?;
    let decoded = Value::parse_json(&json).map_err(|_| DecodeError::Fallback)?;
    let mut map = match decoded {
        Value::Map(m) => m,
        Value::List(_) => return Err(DecodeError::Fallback),
        other => {
            return Err(DecodeError::Fatal(format!(
                "'{}' object has no attribute 'pop'",
                type_name(&other)
            )));
        }
    };
    let encoded_keys = map.shift_remove("boto_encoded_keys").unwrap_or(Value::Null);
    let keys: Vec<Value> = match encoded_keys {
        Value::Null => return Ok(Value::Map(map)),
        Value::List(l) => l,
        Value::Str(s) => s.chars().map(|c| Value::Str(c.to_string())).collect(),
        Value::Map(m) => m.keys().cloned().map(Value::Str).collect(),
        _ => return Err(DecodeError::Fallback),
    };
    let mut token = Value::Map(map);
    for key in keys {
        let path: Vec<Value> = match key {
            Value::List(l) => l,
            Value::Str(s) => s.chars().map(|c| Value::Str(c.to_string())).collect(),
            _ => return Err(DecodeError::Fallback),
        };
        let encoded = match path_get(&token, &path)? {
            Value::Str(s) => s.clone(),
            other => {
                return Err(DecodeError::Fatal(format!(
                    "'{}' object has no attribute 'encode'",
                    type_name(other)
                )));
            }
        };
        let decoded = a2b_base64(encoded.as_bytes()).ok_or(DecodeError::Fallback)?;
        let Some((last, parent)) = path.split_last() else {
            return Err(DecodeError::Fatal("list index out of range".into()));
        };
        let parent: Vec<Value> = parent.to_vec();
        path_set(&mut token, &parent, last, Value::Bytes(decoded))?;
    }
    Ok(token)
}

/// One step of `TokenDecoder._path_get`.
fn step<'a>(d: &'a Value, step: &Value) -> Result<&'a Value, DecodeError> {
    match (d, step) {
        (Value::Map(m), Value::Str(k)) => m
            .get(k)
            .ok_or_else(|| DecodeError::Fatal(pyfmt::py_repr(step))),
        (Value::Map(_), _) => Err(DecodeError::Fatal(pyfmt::py_repr(step))),
        (Value::List(l), Value::Int(i)) => {
            let i = i.to_i64().unwrap_or(i64::MAX);
            let len = l.len() as i64;
            let idx = if i < 0 { len + i } else { i };
            if idx < 0 || idx >= len {
                Err(DecodeError::Fatal("list index out of range".into()))
            } else {
                Ok(&l[idx as usize])
            }
        }
        _ => Err(DecodeError::Fallback),
    }
}

fn path_get<'a>(data: &'a Value, path: &[Value]) -> Result<&'a Value, DecodeError> {
    let mut d = data;
    for s in path {
        d = step(d, s)?;
    }
    Ok(d)
}

/// `TokenDecoder._path_set`.
fn path_set(
    data: &mut Value,
    parent: &[Value],
    key: &Value,
    value: Value,
) -> Result<(), DecodeError> {
    path_get(data, parent)?;
    let mut container = data;
    for s in parent {
        container = match (container, s) {
            (Value::Map(m), Value::Str(k)) => m.get_mut(k).unwrap(),
            (Value::List(l), Value::Int(i)) => {
                let len = l.len() as i64;
                let i = i.to_i64().unwrap_or(0);
                let idx = if i < 0 { len + i } else { i };
                &mut l[idx as usize]
            }
            _ => unreachable!(),
        };
    }
    match (container, key) {
        (Value::Map(m), Value::Str(k)) => {
            m.insert(k.clone(), value);
            Ok(())
        }
        (Value::List(l), Value::Int(i)) => {
            let len = l.len() as i64;
            let i = i.to_i64().unwrap_or(i64::MAX);
            let idx = if i < 0 { len + i } else { i };
            if idx < 0 || idx >= len {
                return Err(DecodeError::Fatal(
                    "list assignment index out of range".into(),
                ));
            }
            l[idx as usize] = value;
            Ok(())
        }
        _ => Err(DecodeError::Fallback),
    }
}

/// Python `int(s)` for a decimal string.
fn py_int(s: &str) -> Option<i64> {
    let t = s.trim();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.chars().all(|c| c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    let n: i64 = digits.replace('_', "").parse().ok()?;
    Some(if neg { -n } else { n })
}

fn as_str_or_list(v: Option<&Node>) -> Vec<String> {
    let Some(v) = v else {
        return Vec::new();
    };
    if let Some(a) = v.as_array() {
        return a
            .iter()
            .map(|x| x.as_str().unwrap_or_default().to_string())
            .collect();
    }
    v.as_str().map(|s| vec![s.to_string()]).unwrap_or_default()
}

/// botocore `PageIterator`, driven one page at a time. The work Python
/// does after a `yield` (counting items, computing the next token,
/// setting the resume token, the duplicate token check) runs at the start
/// of the following [`PageIterator::next_page`] call, so pages, side
/// effects, and errors come out in the same order as in Python.
pub struct PageIterator {
    input_token: Vec<String>,
    output_token: Vec<Expr>,
    more_results: Option<Expr>,
    result_keys: Option<Vec<Expr>>,
    non_aggregate_keys: Vec<Expr>,
    limit_key: Option<String>,
    max_items: Option<i64>,
    starting_token: Option<String>,
    page_size: Option<Value>,
    op_kwargs: Map,
    resume_token: Option<String>,
    non_aggregate_part: Map,
    started: bool,
    done: bool,
    first_request: bool,
    total_items: i64,
    starting_truncation: Value,
    next_token: Map,
    previous_next_token: Option<Map>,
    pending: Option<(Value, i64)>,
}

impl PageIterator {
    /// `Paginator.__init__` + `Paginator.paginate` (with
    /// `_extract_paging_params`). `limit_key_type` is the type name of the
    /// operation's input member named by `limit_key` (see
    /// [`limit_key_type`]); a `string` limit key sends `PageSize` as a
    /// string.
    pub fn new(
        paginator: &Obj,
        params: &Value,
        config: &PaginationConfig,
        limit_key_type: Option<&str>,
    ) -> Result<PageIterator> {
        let output_token = as_str_or_list(paginator.get("output_token"))
            .iter()
            .map(|s| Expr::compile(s))
            .collect::<Result<_>>()?;
        let input_token = as_str_or_list(paginator.get("input_token"));
        let more_results = paginator
            .get("more_results")
            .and_then(|m| m.as_str())
            .map(Expr::compile)
            .transpose()?;
        let non_aggregate_keys = as_str_or_list(paginator.get("non_aggregate_keys"))
            .iter()
            .map(|s| Expr::compile(s))
            .collect::<Result<_>>()?;
        let result_keys = match paginator.get("result_key") {
            None => None,
            Some(v) if v.is_null() => None,
            v => Some(
                as_str_or_list(v)
                    .iter()
                    .map(|s| Expr::compile(s))
                    .collect::<Result<_>>()?,
            ),
        };
        let limit_key = paginator
            .get("limit_key")
            .and_then(|l| l.as_str())
            .map(str::to_string);
        let page_size = match config.page_size {
            None => None,
            Some(size) => {
                if limit_key.is_none() {
                    return Err(PaginateError::Pagination(
                        "PageSize parameter is not supported for the pagination interface for this operation.".into(),
                    )
                    .into());
                }
                Some(if limit_key_type == Some("string") {
                    Value::Str(size.to_string())
                } else {
                    Value::int(size)
                })
            }
        };
        let mut op_kwargs = params.as_map().cloned().unwrap_or_default();
        op_kwargs.shift_remove("PaginationConfig");
        Ok(PageIterator {
            input_token,
            output_token,
            more_results,
            result_keys,
            non_aggregate_keys,
            limit_key,
            max_items: config.max_items,
            starting_token: config.starting_token.clone(),
            page_size,
            op_kwargs,
            resume_token: None,
            non_aggregate_part: Map::new(),
            started: false,
            done: false,
            first_request: true,
            total_items: 0,
            starting_truncation: Value::int(0),
            next_token: Map::new(),
            previous_next_token: None,
            pending: None,
        })
    }

    /// The `result_key` expressions.
    pub fn result_keys(&self) -> Vec<String> {
        self.result_keys
            .iter()
            .flatten()
            .map(|e| e.source.clone())
            .collect()
    }

    /// `PageIterator.resume_token`.
    pub fn resume_token(&self) -> Option<&str> {
        self.resume_token.as_deref()
    }

    /// `PageIterator.non_aggregate_part`.
    pub fn non_aggregate_part(&self) -> &Map {
        &self.non_aggregate_part
    }

    /// The `resume_token` setter.
    fn set_resume_token(&mut self, value: &Map) -> Result<()> {
        let mut token_keys = self.input_token.clone();
        if value.contains_key("boto_truncate_amount") {
            token_keys.push("boto_truncate_amount".into());
        }
        token_keys.sort();
        let mut dict_keys: Vec<String> = value.keys().cloned().collect();
        dict_keys.sort();
        if token_keys == dict_keys {
            self.resume_token = Some(encode_token(value));
            Ok(())
        } else {
            Err(PaginateError::Value(format!(
                "Bad starting token: {}",
                pyfmt::py_repr(&Value::Map(value.clone()))
            ))
            .into())
        }
    }

    /// `_parse_starting_token` (the token is present).
    fn parse_starting_token(&self) -> Result<(Map, Value)> {
        let token = self.starting_token.as_deref().unwrap_or_default();
        match decode_token(token) {
            Ok(Value::Map(mut m)) => {
                let mut index = Value::int(0);
                if m.contains_key("boto_truncate_amount") {
                    index = m.shift_remove("boto_truncate_amount").unwrap();
                }
                Ok((m, index))
            }
            Ok(_) => unreachable!(),
            Err(DecodeError::Fatal(msg)) => Err(internal(msg)),
            Err(DecodeError::Fallback) => self.parse_starting_token_deprecated(token),
        }
    }

    /// `_parse_starting_token_deprecated` and
    /// `_convert_deprecated_starting_token`.
    fn parse_starting_token_deprecated(&self, token: &str) -> Result<(Map, Value)> {
        let mut parts: Vec<String> = token.split("___").map(str::to_string).collect();
        let mut index = 0;
        if parts.len() == self.input_token.len() + 1 {
            let last = parts.pop().unwrap();
            match py_int(&last) {
                Some(i) => index = i,
                None => parts = vec![token.to_string()],
            }
        }
        let mut values: Vec<Value> = parts
            .into_iter()
            .map(|p| {
                if p == "None" {
                    Value::Null
                } else {
                    Value::Str(p)
                }
            })
            .collect();
        if values.len() > self.input_token.len() {
            return Err(PaginateError::Value(format!("Bad starting token: {token}")).into());
        }
        while values.len() < self.input_token.len() {
            values.push(Value::Null);
        }
        let map = self.input_token.iter().cloned().zip(values).collect();
        Ok((map, Value::int(index)))
    }

    /// `_inject_token_into_kwargs`.
    fn inject_token_into_kwargs(&mut self, next_token: &Map) {
        for (name, token) in next_token {
            if *token != Value::Null && *token != Value::str("None") {
                self.op_kwargs.insert(name.clone(), token.clone());
            } else {
                self.op_kwargs.shift_remove(name);
            }
        }
    }

    /// `_inject_starting_params`.
    fn inject_starting_params(&mut self) -> Result<()> {
        if self.starting_token.is_some() {
            let next_token = self.parse_starting_token()?.0;
            self.inject_token_into_kwargs(&next_token);
        }
        if let Some(size) = self.page_size.clone() {
            let key = self.limit_key.clone().unwrap_or_default();
            self.op_kwargs.insert(key, size);
        }
        Ok(())
    }

    fn primary_result_key(&self) -> Result<&Expr> {
        self.result_keys
            .as_ref()
            .and_then(|k| k.first())
            .ok_or_else(|| internal("'NoneType' object is not subscriptable"))
    }

    /// `_record_non_aggregate_key_values`.
    fn record_non_aggregate_key_values(&mut self, response: &Value) -> Result<()> {
        let mut keys = Map::new();
        for expr in &self.non_aggregate_keys {
            let result = expr.search(response)?;
            jmes::set_value_from_jmespath(&mut keys, &expr.source, result).map_err(internal)?;
        }
        self.non_aggregate_part = keys;
        Ok(())
    }

    /// `_handle_first_request`.
    fn handle_first_request(&self, parsed: &mut Value) -> Result<Value> {
        let starting_truncation = self.parse_starting_token()?.1;
        let primary = self.primary_result_key()?;
        let all_data = primary.search(parsed)?;
        let data = match &all_data {
            Value::List(_) | Value::Str(_) => {
                py_slice(&all_data, slice_index(&starting_truncation)?, None)?
            }
            _ => Value::Null,
        };
        let map = parsed
            .as_map_mut()
            .ok_or_else(|| internal("response is not a dict"))?;
        jmes::set_value_from_jmespath(map, &primary.source, data).map_err(internal)?;
        for token in self.result_keys.iter().flatten().skip(1) {
            let sample = token.search(parsed)?;
            let empty = match sample {
                Value::List(_) => Value::List(Vec::new()),
                Value::Str(_) => Value::str(""),
                Value::Int(_) | Value::Float(_) | Value::Bool(_) => sample,
                _ => Value::Null,
            };
            let map = parsed.as_map_mut().unwrap();
            jmes::set_value_from_jmespath(map, &token.source, empty).map_err(internal)?;
        }
        Ok(starting_truncation)
    }

    /// `_truncate_response`.
    fn truncate_response(&mut self, parsed: &mut Value, truncate_amount: i64) -> Result<()> {
        let primary = self.primary_result_key()?;
        let source = primary.source.clone();
        let mut original = primary.search(parsed)?;
        if original == Value::Null {
            original = Value::List(Vec::new());
        }
        let amount_to_keep = py_len(&original)? - truncate_amount;
        let truncated = py_slice(&original, None, Some(amount_to_keep))?;
        let map = parsed
            .as_map_mut()
            .ok_or_else(|| internal("response is not a dict"))?;
        jmes::set_value_from_jmespath(map, &source, truncated).map_err(internal)?;
        let amount = py_add(&Value::int(amount_to_keep), &self.starting_truncation)?;
        let mut next_token = self.next_token.clone();
        next_token.insert("boto_truncate_amount".into(), amount);
        self.next_token = next_token.clone();
        self.set_resume_token(&next_token)
    }

    /// `_get_next_token`.
    fn get_next_token(&self, parsed: &Value) -> Result<Map> {
        if let Some(more) = &self.more_results
            && !more.search(parsed)?.is_truthy()
        {
            return Ok(Map::new());
        }
        let mut next_tokens = Map::new();
        for (output_token, input_key) in self.output_token.iter().zip(&self.input_token) {
            let token = output_token.search(parsed)?;
            if token.is_truthy() {
                next_tokens.insert(input_key.clone(), token);
            } else {
                next_tokens.insert(input_key.clone(), Value::Null);
            }
        }
        Ok(next_tokens)
    }

    /// The next page of `PageIterator.__iter__`, or `None` when the
    /// iteration ends.
    pub fn next_page(&mut self, call: &mut CallFn<'_>) -> Result<Option<Value>> {
        if self.done {
            return Ok(None);
        }
        if !self.started {
            self.started = true;
            self.next_token = self
                .input_token
                .iter()
                .map(|k| (k.clone(), Value::Null))
                .collect();
            if self.starting_token.is_some() {
                self.next_token = self.parse_starting_token()?.0;
            }
            self.primary_result_key()?;
            self.inject_starting_params()?;
        } else if let Some((parsed, num)) = self.pending.take() {
            self.total_items += num;
            let next_token = self.get_next_token(&parsed)?;
            self.next_token = next_token.clone();
            if next_token.values().all(|t| *t == Value::Null) {
                self.done = true;
                return Ok(None);
            }
            if self.max_items == Some(self.total_items) {
                self.done = true;
                self.set_resume_token(&next_token)?;
                return Ok(None);
            }
            if let Some(prev) = &self.previous_next_token
                && py_eq(&Value::Map(prev.clone()), &Value::Map(next_token.clone()))
            {
                self.done = true;
                return Err(PaginateError::Pagination(format!(
                    "The same next token was received twice: {}",
                    pyfmt::py_repr(&Value::Map(next_token))
                ))
                .into());
            }
            self.inject_token_into_kwargs(&next_token);
            self.previous_next_token = Some(next_token);
        }
        let mut parsed = call(&Value::Map(self.op_kwargs.clone()))?;
        if self.first_request {
            if self.starting_token.is_some() {
                self.starting_truncation = self.handle_first_request(&mut parsed)?;
            }
            self.first_request = false;
            self.record_non_aggregate_key_values(&parsed)?;
        } else {
            self.starting_truncation = Value::int(0);
        }
        let mut current = self.primary_result_key()?.search(&parsed)?;
        if current == Value::Null {
            current = Value::List(Vec::new());
        }
        let num = py_len(&current)?;
        let truncate_amount = match self.max_items {
            Some(max) => self.total_items + num - max,
            None => 0,
        };
        if truncate_amount > 0 {
            self.truncate_response(&mut parsed, truncate_amount)?;
            self.done = true;
            return Ok(Some(parsed));
        }
        self.pending = Some((parsed.clone(), num));
        Ok(Some(parsed))
    }

    /// `PageIterator.build_full_result` over the pages of one iteration.
    pub fn build_full_result(&self, pages: &[Value]) -> Result<Value> {
        let mut complete = Map::new();
        for page in pages {
            for expr in self.result_keys.iter().flatten() {
                let result_value = expr.search(page)?;
                if result_value == Value::Null {
                    continue;
                }
                let existing = expr.search(&Value::Map(complete.clone()))?;
                if existing == Value::Null {
                    jmes::set_value_from_jmespath(&mut complete, &expr.source, result_value)
                        .map_err(internal)?;
                    continue;
                }
                match &result_value {
                    Value::List(items) => match existing {
                        Value::List(mut l) => {
                            l.extend(items.iter().cloned());
                            jmes::set_value_from_jmespath(
                                &mut complete,
                                &expr.source,
                                Value::List(l),
                            )
                            .map_err(internal)?;
                        }
                        other => {
                            return Err(internal(format!(
                                "'{}' object has no attribute 'extend'",
                                type_name(&other)
                            )));
                        }
                    },
                    Value::Int(_) | Value::Float(_) | Value::Str(_) | Value::Bool(_) => {
                        let sum = py_add(&existing, &result_value)?;
                        jmes::set_value_from_jmespath(&mut complete, &expr.source, sum)
                            .map_err(internal)?;
                    }
                    _ => {}
                }
            }
        }
        merge_dicts(&mut complete, &self.non_aggregate_part);
        if let Some(token) = &self.resume_token {
            complete.insert("NextToken".into(), Value::str(token.clone()));
        }
        Ok(Value::Map(complete))
    }
}

/// botocore `utils.merge_dicts(dict1, dict2)`.
fn merge_dicts(dict1: &mut Map, dict2: &Map) {
    for (key, value) in dict2 {
        if let Value::Map(inner) = value {
            if let Some(Value::Map(existing)) = dict1.get_mut(key) {
                merge_dicts(existing, inner);
            } else {
                dict1.insert(key.clone(), value.clone());
            }
        } else {
            dict1.insert(key.clone(), value.clone());
        }
    }
}

/// The type name of the input member named by the paginator's
/// `limit_key` (`awscli/customizations/paginate.py` types `--page-size`
/// and `--max-items` by it; botocore sends a `string` page size as a
/// string).
pub fn limit_key_type<'a>(operation: &Operation<'a>, paginator: &Obj) -> Option<&'a str> {
    let key = paginator.get("limit_key")?.as_str()?;
    Some(operation.input()?.member_shape(key)?.type_name())
}

/// Run a paginator over `call`, starting from `params`, according to the
/// `pagination` entry of `paginators-1.json` and `config`, and consume
/// every page. `limit_key_type` is [`limit_key_type`] of the operation.
pub fn paginate(
    paginator: &Obj,
    params: &Value,
    config: &PaginationConfig,
    limit_key_type: Option<&str>,
    call: &mut CallFn<'_>,
) -> Result<PaginatedResponse> {
    let mut it = PageIterator::new(paginator, params, config, limit_key_type)?;
    let mut pages = Vec::new();
    while let Some(page) = it.next_page(call)? {
        pages.push(page);
    }
    let full_result = it.build_full_result(&pages)?;
    Ok(PaginatedResponse {
        result_keys: it.result_keys(),
        non_aggregate_part: it.non_aggregate_part.clone(),
        resume_token: it.resume_token.clone(),
        pages,
        full_result,
    })
}

/// A `PageIterator` whose pages are requested as the formatter consumes
/// them, as the CLI's formatters iterate botocore's lazy iterator.
pub struct LazyPages<'a> {
    pub iterator: PageIterator,
    pub call: Box<CallFn<'a>>,
}

impl LazyPages<'_> {
    pub fn next_page(&mut self) -> Result<Option<Value>> {
        self.iterator.next_page(&mut *self.call)
    }

    /// Consume every remaining page (`build_full_result`).
    pub fn collect(mut self) -> Result<PaginatedResponse> {
        let mut pages = Vec::new();
        while let Some(page) = self.next_page()? {
            pages.push(page);
        }
        let full_result = self.iterator.build_full_result(&pages)?;
        Ok(PaginatedResponse {
            result_keys: self.iterator.result_keys(),
            non_aggregate_part: self.iterator.non_aggregate_part.clone(),
            resume_token: self.iterator.resume_token.clone(),
            pages,
            full_result,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Produced by the reference botocore `Paginator` / `PageIterator` /
    /// `TokenEncoder` / `TokenDecoder` over scripted pages (bytes are
    /// written as `{"__bytes__": base64}`).
    const GOLDEN: &str = r#"{"scenarios": [{"name": "basic", "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "calls": [{"Filter": "x"}, {"Filter": "x", "NextToken": "t1"}, {"Filter": "x", "NextToken": "t2"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [1, 2, 3, 4, 5]}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "params": {"Filter": "x"}}}, {"name": "max_items_mid_page", "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3], "NextToken": "t2"}], "calls": [{}, {"NextToken": "t1"}], "resume_token": "eyJOZXh0VG9rZW4iOiAidDEiLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAxfQ==", "non_aggregate_part": {}, "full_result": {"Items": [1, 2, 3], "NextToken": "eyJOZXh0VG9rZW4iOiAidDEiLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAxfQ=="}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"MaxItems": 3}}}, {"name": "max_items_boundary", "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}], "calls": [{}, {"NextToken": "t1"}], "resume_token": "eyJOZXh0VG9rZW4iOiAidDIifQ==", "non_aggregate_part": {}, "full_result": {"Items": [1, 2, 3, 4], "NextToken": "eyJOZXh0VG9rZW4iOiAidDIifQ=="}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"MaxItems": 4}}}, {"name": "max_items_first_page", "pages": [{"Items": [1], "NextToken": "t1"}], "calls": [{}], "resume_token": "eyJOZXh0VG9rZW4iOiBudWxsLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAxfQ==", "non_aggregate_part": {}, "full_result": {"Items": [1], "NextToken": "eyJOZXh0VG9rZW4iOiBudWxsLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAxfQ=="}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"MaxItems": 1}}}, {"name": "starting_token_truncated", "pages": [{"Items": [4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "calls": [{"NextToken": "t1"}, {"NextToken": "t2"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [4, 5]}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"StartingToken": "eyJOZXh0VG9rZW4iOiAidDEiLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAxfQ=="}}}, {"name": "starting_token_and_max", "pages": [{"Items": [4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "calls": [{"NextToken": "t1"}, {"NextToken": "t2"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [4, 5]}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"StartingToken": "eyJOZXh0VG9rZW4iOiAidDEiLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAxfQ==", "MaxItems": 2}}}, {"name": "page_size_int", "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "calls": [{"MaxResults": 2}, {"MaxResults": 2, "NextToken": "t1"}, {"MaxResults": 2, "NextToken": "t2"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [1, 2, 3, 4, 5]}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"PageSize": 2}}}, {"name": "page_size_str", "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "calls": [{"MaxResults": "2"}, {"MaxResults": "2", "NextToken": "t1"}, {"MaxResults": "2", "NextToken": "t2"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [1, 2, 3, 4, 5]}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"PageSize": 2}, "limit_type": "string"}}, {"name": "page_size_unsupported", "error": "Error during pagination: PageSize parameter is not supported for the pagination interface for this operation.", "error_type": "PaginationError", "calls": [], "input": {"paginator": {"input_token": "Marker", "output_token": "NextMarker", "result_key": "Items"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"PageSize": 2}}}, {"name": "multi_keys", "pages": [{"Items": ["a"], "Count": 1, "Name": "x", "Data": {"Rows": [1]}, "Meta": {"Owner": "o", "Other": 1}, "Total": 9, "NextToken": "n"}, {"Items": ["b", "c"], "Count": 2, "Name": "y", "Data": {"Rows": [2]}, "Total": 10}], "calls": [{}, {"NextToken": "n"}], "resume_token": null, "non_aggregate_part": {"Meta": {"Owner": "o"}, "Total": 9}, "full_result": {"Items": ["a", "b", "c"], "Count": 3, "Name": "xy", "Data": {"Rows": [1, 2]}, "Meta": {"Owner": "o"}, "Total": 9}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": ["Items", "Count", "Name", "Data.Rows"], "non_aggregate_keys": ["Meta.Owner", "Total"]}, "pages": [{"Items": ["a"], "Count": 1, "Name": "x", "Data": {"Rows": [1]}, "Meta": {"Owner": "o", "Other": 1}, "Total": 9, "NextToken": "n"}, {"Items": ["b", "c"], "Count": 2, "Name": "y", "Data": {"Rows": [2]}, "Total": 10}]}}, {"name": "multi_keys_starting_token", "pages": [{"Items": ["c"], "Count": 3, "Name": "", "Flag": true}], "calls": [{"NextToken": "q"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": ["c"], "Count": 3, "Name": "", "Flag": true}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": ["Items", "Count", "Name", "Flag"]}, "pages": [{"Items": ["a", "b", "c"], "Count": 3, "Name": "x", "Flag": true}], "config": {"StartingToken": "eyJOZXh0VG9rZW4iOiAicSIsICJib3RvX3RydW5jYXRlX2Ftb3VudCI6IDJ9"}}}, {"name": "more_results", "pages": [{"Items": [1], "IsTruncated": true, "NextMarker": "m1"}, {"Items": [2], "IsTruncated": false, "NextMarker": "m2"}], "calls": [{}, {"Marker": "m1"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [1, 2]}, "input": {"paginator": {"input_token": "Marker", "output_token": "NextMarker", "more_results": "IsTruncated", "result_key": "Items"}, "pages": [{"Items": [1], "IsTruncated": true, "NextMarker": "m1"}, {"Items": [2], "IsTruncated": false, "NextMarker": "m2"}]}}, {"name": "duplicate_token", "error": "Error during pagination: The same next token was received twice: {'NextToken': 'b'}", "error_type": "PaginationError", "calls": [{}, {"NextToken": "a"}, {"NextToken": "b"}], "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1], "NextToken": "a"}, {"Items": [2], "NextToken": "b"}, {"Items": [3], "NextToken": "b"}, {"Items": [4]}]}}, {"name": "empty_string_token", "pages": [{"Items": [1], "NextToken": ""}], "calls": [{}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [1]}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1], "NextToken": ""}, {"Items": [2]}]}}, {"name": "multi_input_tokens", "pages": [{"Items": [1, 2], "NextA": "a1"}], "calls": [{}], "resume_token": "eyJBIjogImExIiwgIkIiOiBudWxsfQ==", "non_aggregate_part": {}, "full_result": {"Items": [1, 2], "NextToken": "eyJBIjogImExIiwgIkIiOiBudWxsfQ=="}, "input": {"paginator": {"input_token": ["A", "B"], "output_token": ["NextA", "NextB"], "result_key": "Items", "limit_key": "Max"}, "pages": [{"Items": [1, 2], "NextA": "a1"}, {"Items": [3], "NextA": "a2", "NextB": "b2"}, {"Items": [4]}], "config": {"MaxItems": 2}}}, {"name": "multi_input_tokens_mid", "pages": [{"Items": [1, 2], "NextA": "a1"}, {"Items": [3], "NextA": "a2", "NextB": "b2"}], "calls": [{}, {"A": "a1"}], "resume_token": "eyJBIjogImExIiwgIkIiOiBudWxsLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAxfQ==", "non_aggregate_part": {}, "full_result": {"Items": [1, 2, 3], "NextToken": "eyJBIjogImExIiwgIkIiOiBudWxsLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAxfQ=="}, "input": {"paginator": {"input_token": ["A", "B"], "output_token": ["NextA", "NextB"], "result_key": "Items"}, "pages": [{"Items": [1, 2], "NextA": "a1"}, {"Items": [3, 4], "NextA": "a2", "NextB": "b2"}, {"Items": [4]}], "config": {"MaxItems": 3}}}, {"name": "legacy_token", "pages": [{"Items": [4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "calls": [{"NextToken": "t1"}, {"NextToken": "t2"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [4, 5]}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"StartingToken": "t1___1"}}}, {"name": "legacy_token_opaque", "pages": [{"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "calls": [{"NextToken": "opaque-token"}, {"NextToken": "t2"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [3, 4, 5]}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"StartingToken": "opaque-token"}}}, {"name": "legacy_token_badint", "pages": [{"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "calls": [{"NextToken": "t1___x"}, {"NextToken": "t2"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [3, 4, 5]}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"StartingToken": "t1___x"}}}, {"name": "legacy_multi", "pages": [{"Items": [2], "NextToken": "t1"}], "calls": [{"B": "b"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [2]}, "input": {"paginator": {"input_token": ["A", "B"], "output_token": ["NextA", "NextB"], "result_key": "Items"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"StartingToken": "None___b___1"}}}, {"name": "legacy_multi_short", "pages": [{"Items": [1, 2], "NextToken": "t1"}], "calls": [{"A": "a"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [1, 2]}, "input": {"paginator": {"input_token": ["A", "B"], "output_token": ["NextA", "NextB"], "result_key": "Items"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"StartingToken": "a"}}}, {"name": "legacy_too_many", "error": "Bad starting token: a___b___c", "error_type": "ValueError", "calls": [], "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"StartingToken": "a___b___c"}}}, {"name": "token_int_json", "error": "'int' object has no attribute 'pop'", "error_type": "AttributeError", "calls": [], "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"StartingToken": "MTIz"}}}, {"name": "token_list_json", "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "calls": [{"NextToken": "WzFd"}, {"NextToken": "t1"}, {"NextToken": "t2"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [1, 2, 3, 4, 5]}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"StartingToken": "WzFd"}}}, {"name": "token_bad_keys", "error": "Bad starting token: {'Other': 'x', 'boto_truncate_amount': 1}", "error_type": "ValueError", "calls": [{"Other": "x"}], "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"StartingToken": "eyJPdGhlciI6ICJ4In0=", "MaxItems": 1}}}, {"name": "dict_tokens", "pages": [{"Items": [{"k": {"S": "1"}}, {"k": {"S": "2"}}], "Count": 2, "ScannedCount": 5, "LastEvaluatedKey": {"k": {"S": "2"}}}, {"Items": [{"k": {"S": "3"}}], "Count": 1, "ScannedCount": 1}], "calls": [{}, {"ExclusiveStartKey": {"k": {"S": "2"}}}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [{"k": {"S": "1"}}, {"k": {"S": "2"}}, {"k": {"S": "3"}}], "Count": 3, "ScannedCount": 6}, "input": {"paginator": {"input_token": "ExclusiveStartKey", "output_token": "LastEvaluatedKey", "result_key": ["Items", "Count", "ScannedCount"], "limit_key": "Limit"}, "pages": [{"Items": [{"k": {"S": "1"}}, {"k": {"S": "2"}}], "Count": 2, "ScannedCount": 5, "LastEvaluatedKey": {"k": {"S": "2"}}}, {"Items": [{"k": {"S": "3"}}], "Count": 1, "ScannedCount": 1}], "config": {"MaxItems": 3}}}, {"name": "bytes_token", "pages": [{"Items": [1], "NextToken": {"__bytes__": "AP9iaW4="}}], "calls": [{}], "resume_token": "eyJOZXh0VG9rZW4iOiBudWxsLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAxfQ==", "non_aggregate_part": {}, "full_result": {"Items": [1], "NextToken": "eyJOZXh0VG9rZW4iOiBudWxsLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAxfQ=="}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1, 2], "NextToken": {"__bytes__": "AP9iaW4="}}, {"Items": [3]}], "config": {"MaxItems": 1}}}, {"name": "bytes_token_resume", "pages": [{"Items": [4]}], "calls": [{"NextToken": {"__bytes__": "AP9iaW4="}}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [4]}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [3, 4]}], "config": {"StartingToken": "eyJOZXh0VG9rZW4iOiAiQVA5aWFXND0iLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAxLCAiYm90b19lbmNvZGVkX2tleXMiOiBbWyJOZXh0VG9rZW4iXV19"}}}, {"name": "no_result_key_value", "pages": [{"NextToken": "a"}, {"Items": [1]}], "calls": [{}, {"NextToken": "a"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Items": [1]}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"NextToken": "a"}, {"Items": [1]}], "config": {"MaxItems": 5}}}, {"name": "string_result", "pages": [{"Text": "abc", "NextToken": "n"}, {"Text": "d"}], "calls": [{}, {"NextToken": "n"}], "resume_token": "eyJOZXh0VG9rZW4iOiAibiIsICJib3RvX3RydW5jYXRlX2Ftb3VudCI6IDF9", "non_aggregate_part": {}, "full_result": {"Text": "abcd", "NextToken": "eyJOZXh0VG9rZW4iOiAibiIsICJib3RvX3RydW5jYXRlX2Ftb3VudCI6IDF9"}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Text"}, "pages": [{"Text": "abc", "NextToken": "n"}, {"Text": "def"}], "config": {"MaxItems": 4}}}, {"name": "complex_output_token", "pages": [{"Contents": [{"Key": "k1"}, {"Key": "k2"}], "IsTruncated": true}, {"Contents": [{"Key": "k3"}], "IsTruncated": false}], "calls": [{}, {"Marker": "k2"}], "resume_token": null, "non_aggregate_part": {}, "full_result": {"Contents": [{"Key": "k1"}, {"Key": "k2"}, {"Key": "k3"}]}, "input": {"paginator": {"input_token": "Marker", "output_token": "NextMarker || Contents[-1].Key", "more_results": "IsTruncated", "result_key": "Contents"}, "pages": [{"Contents": [{"Key": "k1"}, {"Key": "k2"}], "IsTruncated": true}, {"Contents": [{"Key": "k3"}], "IsTruncated": false}]}}, {"name": "non_positive_max", "pages": [{"Items": [], "NextToken": "t1"}], "calls": [{}], "resume_token": "eyJOZXh0VG9rZW4iOiBudWxsLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAwfQ==", "non_aggregate_part": {}, "full_result": {"Items": [], "NextToken": "eyJOZXh0VG9rZW4iOiBudWxsLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAwfQ=="}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"MaxItems": 0}}}, {"name": "negative_max", "pages": [{"Items": [1], "NextToken": "t1"}], "calls": [{}], "resume_token": "eyJOZXh0VG9rZW4iOiBudWxsLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAtMX0=", "non_aggregate_part": {}, "full_result": {"Items": [1], "NextToken": "eyJOZXh0VG9rZW4iOiBudWxsLCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAtMX0="}, "input": {"paginator": {"input_token": "NextToken", "output_token": "NextToken", "result_key": "Items", "limit_key": "MaxResults"}, "pages": [{"Items": [1, 2], "NextToken": "t1"}, {"Items": [3, 4], "NextToken": "t2"}, {"Items": [5], "ResponseMetadata": {"RequestId": "r"}}], "config": {"MaxItems": -1}}}], "tokens": [[{"NextToken": "abc"}, "eyJOZXh0VG9rZW4iOiAiYWJjIn0="], [{"A": null, "B": "\u00e9\u2603"}, "eyJBIjogbnVsbCwgIkIiOiAiXHUwMGU5XHUyNjAzIn0="], [{"K": {"x": [1, 2.5, true]}, "boto_truncate_amount": 3}, "eyJLIjogeyJ4IjogWzEsIDIuNSwgdHJ1ZV19LCAiYm90b190cnVuY2F0ZV9hbW91bnQiOiAzfQ=="], [{"T": {"__bytes__": "AAE="}}, "eyJUIjogIkFBRT0iLCAiYm90b19lbmNvZGVkX2tleXMiOiBbWyJUIl1dfQ=="], [{"L": [{"__bytes__": "YQ=="}, {"m": {"__bytes__": "Yg=="}}]}, "eyJMIjogWyJZUT09IiwgeyJtIjogIllnPT0ifV0sICJib3RvX2VuY29kZWRfa2V5cyI6IFtbIkwiLCAwXSwgWyJMIiwgMSwgIm0iXV19"]], "decode": [["QQ==QQ", {"__error__": "JSONDecodeError"}], ["eyJBIjogMX0", {"__error__": "Error"}], ["eyJBIjogMX0=", {"A": 1}], ["eyJBIjogMX0==x", {"__error__": "JSONDecodeError"}], ["e30=", {}], ["eyJUIjogIkFBRT0iLCAiYm90b19lbmNvZGVkX2tleXMiOiBbWyJUIl1dfQ==", {"T": {"__bytes__": "AAE="}}]]}"#; // gitleaks:allow

    fn dec(v: &serde_json::Value) -> Value {
        match v {
            serde_json::Value::Object(o) if o.len() == 1 && o.contains_key("__bytes__") => {
                Value::Bytes(
                    base64::engine::general_purpose::STANDARD
                        .decode(o["__bytes__"].as_str().unwrap())
                        .unwrap(),
                )
            }
            serde_json::Value::Object(o) => {
                Value::Map(o.iter().map(|(k, v)| (k.clone(), dec(v))).collect())
            }
            serde_json::Value::Array(a) => Value::List(a.iter().map(dec).collect()),
            other => Value::from_json(other),
        }
    }

    fn show(v: &Value) -> String {
        fn plain(v: &Value) -> Value {
            match v {
                Value::Bytes(b) => Value::str(format!("bytes:{}", pyfmt::bytes_repr(b))),
                Value::List(l) => Value::List(l.iter().map(plain).collect()),
                Value::Map(m) => Value::Map(m.iter().map(|(k, v)| (k.clone(), plain(v))).collect()),
                other => other.clone(),
            }
        }
        pyfmt::json_dumps_default(&plain(v))
    }

    #[test]
    fn scenarios_match_botocore() {
        let golden: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        assert_eq!(golden["scenarios"].as_array().unwrap().len(), 33);
        for sc in golden["scenarios"].as_array().unwrap() {
            let name = sc["name"].as_str().unwrap();
            let input = &sc["input"];
            let paginator = Node::leak(&input["paginator"]).as_object().unwrap();
            let params = dec(input.get("params").unwrap_or(&serde_json::json!({})));
            let cfg = input
                .get("config")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            let config = PaginationConfig {
                starting_token: cfg
                    .get("StartingToken")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                max_items: cfg.get("MaxItems").and_then(|v| v.as_i64()),
                page_size: cfg.get("PageSize").and_then(|v| v.as_i64()),
            };
            let limit_type = input
                .get("limit_type")
                .and_then(|v| v.as_str())
                .unwrap_or("integer");
            let mut queue: Vec<Value> = match dec(&input["pages"]) {
                Value::List(l) => l,
                _ => unreachable!(),
            };
            queue.reverse();
            let mut calls = Vec::new();
            let mut call = |kwargs: &Value| -> Result<Value> {
                calls.push(kwargs.clone());
                queue.pop().ok_or_else(|| anyhow::anyhow!("no more pages"))
            };
            let result = paginate(paginator, &params, &config, Some(limit_type), &mut call);
            let want_calls = show(&dec(&sc["calls"]));
            assert_eq!(show(&Value::List(calls)), want_calls, "{name}: calls");
            if let Some(err) = sc.get("error") {
                let got = result.expect_err(name);
                assert_eq!(got.to_string(), err.as_str().unwrap(), "{name}: error");
                continue;
            }
            let got = result.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                show(&Value::List(got.pages.clone())),
                show(&dec(&sc["pages"])),
                "{name}: pages"
            );
            assert_eq!(
                got.resume_token.as_deref(),
                sc["resume_token"].as_str(),
                "{name}: resume token"
            );
            assert_eq!(
                show(&Value::Map(got.non_aggregate_part.clone())),
                show(&dec(&sc["non_aggregate_part"])),
                "{name}: non aggregate part"
            );
            assert_eq!(
                show(&got.full_result),
                show(&dec(&sc["full_result"])),
                "{name}: full result"
            );
        }
    }

    #[test]
    fn tokens_match_botocore() {
        let golden: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        for case in golden["tokens"].as_array().unwrap() {
            let token = match dec(&case[0]) {
                Value::Map(m) => m,
                _ => unreachable!(),
            };
            let encoded = encode_token(&token);
            assert_eq!(encoded, case[1].as_str().unwrap());
            assert_eq!(
                show(&decode_token(&encoded).unwrap()),
                show(&Value::Map(token))
            );
        }
        for case in golden["decode"].as_array().unwrap() {
            let input = case[0].as_str().unwrap();
            match case[1].get("__error__") {
                Some(_) => assert_eq!(decode_token(input), Err(DecodeError::Fallback), "{input}"),
                None => assert_eq!(
                    show(&decode_token(input).unwrap()),
                    show(&dec(&case[1])),
                    "{input}"
                ),
            }
        }
    }

    #[test]
    fn base64_follows_cpython() {
        let ok = |s: &str| a2b_base64(s.as_bytes());
        assert_eq!(ok("QQ=="), Some(b"A".to_vec()));
        assert_eq!(ok("QQ="), None);
        assert_eq!(ok("Q"), None);
        assert_eq!(ok("QQ==QQ"), Some(b"A\x04\x10".to_vec()));
        assert_eq!(ok("QU!JD"), Some(b"ABC".to_vec()));
        assert_eq!(ok("QQ=x="), Some(b"A\x0c".to_vec()));
        assert_eq!(ok("QUI=Q"), Some(b"AB\x10".to_vec()));
        assert_eq!(ok("QUI=QQ=="), None);
        assert_eq!(ok("QQ==Q"), None);
        assert_eq!(ok("=QQ=="), Some(b"A".to_vec()));
    }
}
