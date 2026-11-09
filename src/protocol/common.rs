//! Helpers shared by the serializers and parsers: Python value semantics
//! (`str()`, `repr(float)`, `json.dumps`, `int()`, `float()`), header
//! dictionaries, percent-encoding, and the model lookups botocore performs
//! around every protocol.

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use num_bigint::BigInt;
use num_traits::ToPrimitive;

use crate::model::{Operation, ServiceModel, Shape};
use crate::protocol::HttpResponse;
use crate::value::{Map, Value};

/// The response headers as botocore sees them: urllib3's `HTTPHeaderDict`
/// wrapped in `botocore.awsrequest.HeadersDict`. Lookups are
/// case-insensitive, a repeated header keeps the case and position of its
/// first occurrence, and repeated values are joined with `", "`.
#[derive(Debug, Clone, Default)]
pub struct HeaderDict {
    entries: Vec<(String, String)>,
}

impl HeaderDict {
    pub fn from_response(response: &HttpResponse) -> HeaderDict {
        let mut entries: Vec<(String, String)> = Vec::new();
        for (k, v) in &response.headers {
            match entries.iter_mut().find(|(e, _)| e.eq_ignore_ascii_case(k)) {
                Some((_, existing)) => {
                    existing.push_str(", ");
                    existing.push_str(v);
                }
                None => entries.push((k.clone(), v.clone())),
            }
        }
        HeaderDict { entries }
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// `botocore.utils.lowercase_dict(headers)`.
    pub fn lowercase(&self) -> Value {
        let mut m = Map::new();
        for (k, v) in self.iter() {
            m.insert(k.to_lowercase(), Value::str(v));
        }
        Value::Map(m)
    }
}

/// Python `repr(float)`: the shortest round-trip digits, in fixed notation
/// for decimal exponents in `-4..16` and `d.ddde+XX` otherwise.
pub fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').expect("exponent");
    let exp: i32 = exp.parse().expect("exponent digits");
    let (neg, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                out.push_str(&"0".repeat(int_len - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            out.push_str("0.");
            out.push_str(&"0".repeat((-exp - 1) as usize));
            out.push_str(&digits);
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

/// Python `str(value)` for the scalar values the serializers stringify.
pub fn py_str(v: &Value) -> Result<String> {
    Ok(match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => py_float_repr(*f),
        Value::Str(s) => s.clone(),
        other => bail!("cannot convert {other:?} to a string"),
    })
}

/// `json.dumps(value)` with `ensure_ascii=True`; `compact` selects
/// `separators=(',', ':')` instead of the defaults `(', ', ': ')`.
pub fn json_dumps(v: &Value, compact: bool) -> Result<String> {
    let mut out = String::new();
    dump_into(&mut out, v, compact)?;
    Ok(out)
}

fn dump_into(out: &mut String, v: &Value, compact: bool) -> Result<()> {
    let (item_sep, key_sep) = if compact { (",", ":") } else { (", ", ": ") };
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) => {
            if f.is_nan() {
                out.push_str("NaN");
            } else if f.is_infinite() {
                out.push_str(if *f > 0.0 { "Infinity" } else { "-Infinity" });
            } else {
                out.push_str(&py_float_repr(*f));
            }
        }
        Value::Str(s) => dump_str(out, s),
        Value::Bytes(_) => bail!("Object of type bytes is not JSON serializable"),
        Value::List(l) => {
            out.push('[');
            for (i, item) in l.iter().enumerate() {
                if i > 0 {
                    out.push_str(item_sep);
                }
                dump_into(out, item, compact)?;
            }
            out.push(']');
        }
        Value::Map(m) => {
            out.push('{');
            for (i, (k, item)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(item_sep);
                }
                dump_str(out, k);
                out.push_str(key_sep);
                dump_into(out, item, compact)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// `json.encoder.py_encode_basestring_ascii`.
fn dump_str(out: &mut String, s: &str) {
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
            c if (' '..='~').contains(&c) => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// `botocore.utils.percent_encode`: `urllib.parse.quote(str(value),
/// safe=safe)` over the UTF-8 bytes. Letters, digits, and `_.-~` are
/// never encoded.
pub fn percent_encode(input: &str, safe: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        let c = b as char;
        if b.is_ascii_alphanumeric() || "_.-~".contains(c) || (b < 0x80 && safe.contains(c)) {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `Serializer._get_base64`.
pub fn b64encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The bytes of a blob or string value (`str` is UTF-8 encoded).
pub fn value_bytes(v: &Value) -> Result<Vec<u8>> {
    match v {
        Value::Str(s) => Ok(s.as_bytes().to_vec()),
        Value::Bytes(b) => Ok(b.clone()),
        other => bail!("expected a blob value, got {other:?}"),
    }
}

/// `base64.b64decode(value)`: a `str` must be ASCII, then
/// `binascii.a2b_base64` without strict mode.
pub fn b64decode(text: &str) -> Result<Vec<u8>> {
    if !text.is_ascii() {
        bail!("string argument should contain only ASCII characters");
    }
    a2b_base64(text.as_bytes()).map_err(|e| anyhow!(e))
}

/// `binascii.a2b_base64(data, strict_mode=False)` (CPython 3.14): characters
/// outside the alphabet are skipped, `=` pads only after two characters of
/// a quantum and is skipped elsewhere, and decoding continues past
/// complete padding.
pub fn a2b_base64(data: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(data.len() / 4 * 3 + 3);
    let (mut quad_pos, mut pads, mut left) = (0u8, 0u8, 0u8);
    for &c in data {
        if c == b'=' {
            pads = pads.saturating_add(1);
            continue;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        pads = 0;
        match quad_pos {
            0 => {
                quad_pos = 1;
                left = v;
            }
            1 => {
                quad_pos = 2;
                out.push((left << 2) | (v >> 4));
                left = v & 0x0f;
            }
            2 => {
                quad_pos = 3;
                out.push((left << 4) | (v >> 2));
                left = v & 0x03;
            }
            _ => {
                quad_pos = 0;
                out.push((left << 6) | v);
                left = 0;
            }
        }
    }
    if quad_pos == 1 {
        return Err(format!(
            "Invalid base64-encoded string: number of data characters ({}) cannot be 1 more than a multiple of 4",
            out.len() / 3 * 4 + 1
        ));
    }
    if quad_pos != 0 && u32::from(quad_pos) + u32::from(pads) < 4 {
        return Err("Incorrect padding".to_string());
    }
    Ok(out)
}

/// Python `int(value)` for a header value or a JSON scalar.
pub fn py_int(v: &Value) -> Result<Value> {
    match v {
        Value::Int(_) => Ok(v.clone()),
        Value::Bool(b) => Ok(Value::int(i64::from(*b))),
        Value::Float(f) => {
            if !f.is_finite() {
                bail!("cannot convert float {} to integer", py_float_repr(*f));
            }
            let i: BigInt = num_traits::FromPrimitive::from_f64(f.trunc()).expect("finite float");
            Ok(Value::Int(i))
        }
        Value::Str(s) => {
            let cleaned = clean_numeric(s.trim(), false)
                .ok_or_else(|| anyhow!("invalid literal for int() with base 10: {s:?}"))?;
            cleaned
                .parse::<BigInt>()
                .map(Value::Int)
                .map_err(|_| anyhow!("invalid literal for int() with base 10: {s:?}"))
        }
        other => bail!("int() argument must be a string or a number, not {other:?}"),
    }
}

/// Python `float(value)` for a header value or a JSON scalar.
pub fn py_float(v: &Value) -> Result<Value> {
    match v {
        Value::Float(_) => Ok(v.clone()),
        Value::Bool(b) => Ok(Value::Float(if *b { 1.0 } else { 0.0 })),
        Value::Int(i) => i
            .to_f64()
            .filter(|f| f.is_finite())
            .map(Value::Float)
            .ok_or_else(|| anyhow!("int too large to convert to float")),
        Value::Str(s) => parse_py_float(s)
            .map(Value::Float)
            .ok_or_else(|| anyhow!("could not convert string to float: {s:?}")),
        other => bail!("float() argument must be a string or a real number, not {other:?}"),
    }
}

/// Python `float(str)`: surrounding whitespace, an optional sign,
/// underscores between digits, and `nan` / `inf` / `infinity` in any case.
pub fn parse_py_float(s: &str) -> Option<f64> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let unsigned = lower.trim_start_matches(['+', '-']);
    if lower.len() - unsigned.len() > 1 {
        return None;
    }
    if matches!(unsigned, "nan" | "inf" | "infinity") {
        return lower.parse::<f64>().ok();
    }
    let cleaned = clean_numeric(t, true)?;
    cleaned.parse::<f64>().ok()
}

/// Validate a Python numeric literal's characters and drop the
/// underscores that sit between two digits.
fn clean_numeric(t: &str, allow_float: bool) -> Option<String> {
    let bytes = t.as_bytes();
    let mut out = String::with_capacity(t.len());
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'0'..=b'9' => out.push(b as char),
            b'_' => {
                let prev = i.checked_sub(1).map(|p| bytes[p].is_ascii_digit());
                let next = bytes.get(i + 1).map(u8::is_ascii_digit);
                if prev != Some(true) || next != Some(true) {
                    return None;
                }
            }
            b'+' | b'-' => out.push(b as char),
            b'.' | b'e' | b'E' if allow_float => out.push(b as char),
            _ => return None,
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// `Serializer._handle_float`: non-finite floats become the strings
/// `Infinity`, `-Infinity`, `NaN`; everything else is unchanged.
pub fn handle_float(v: &Value) -> Value {
    match v {
        Value::Float(f) if f.is_nan() => Value::str("NaN"),
        Value::Float(f) if *f == f64::INFINITY => Value::str("Infinity"),
        Value::Float(f) if *f == f64::NEG_INFINITY => Value::str("-Infinity"),
        other => other.clone(),
    }
}

/// `Serializer._expand_host_prefix`.
pub fn expand_host_prefix(operation: &Operation<'_>, params: &Value) -> Result<Option<String>> {
    let Some(expr) = operation
        .get("endpoint")
        .and_then(|e| e.get("hostPrefix"))
        .and_then(|h| h.as_str())
    else {
        return Ok(None);
    };
    let Some(input) = operation.input() else {
        return Ok(Some(expr.to_string()));
    };
    let mut out = expr.to_string();
    for (name, member) in input.members() {
        if !member.get_bool("hostLabel") {
            continue;
        }
        let value = params
            .get(name)
            .ok_or_else(|| anyhow!("missing host label parameter {name}"))?;
        out = out.replace(&format!("{{{name}}}"), &py_str(value)?);
    }
    Ok(Some(out))
}

/// `ServiceModel.is_query_compatible`.
pub fn is_query_compatible(model: &ServiceModel) -> bool {
    model.metadata().contains_key("awsQueryCompatible")
}

/// `botocore.utils.is_json_value_header`.
pub fn is_json_value_header(shape: &Shape<'_>) -> bool {
    shape.get_bool("jsonvalue")
        && shape.get_str("location") == Some("header")
        && shape.type_name() == "string"
}

/// `StructureShape.event_stream_name`.
pub fn event_stream_name<'a>(shape: &Shape<'a>) -> Option<&'a str> {
    shape
        .members()
        .into_iter()
        .find(|(_, m)| m.get_bool("eventstream"))
        .map(|(n, _)| n)
}

/// `OperationModel.has_event_stream_output`.
pub fn has_event_stream_output(operation: &Operation<'_>) -> bool {
    operation
        .output()
        .is_some_and(|o| event_stream_name(&o).is_some())
}

/// `ServiceModel.shape_for_error_code`: the last exception shape (in model
/// order) whose `error.code`, or name when no code is modeled, equals
/// `code`.
pub fn shape_for_error_code<'a>(model: &'a ServiceModel, code: &str) -> Option<Shape<'a>> {
    let mut found = None;
    for name in model.shape_names() {
        let Some(shape) = model.shape(name) else {
            continue;
        };
        if !shape
            .shape_get("exception")
            .and_then(|e| e.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        let error_code = shape
            .shape_get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            .filter(|c| !c.is_empty())
            .unwrap_or(shape.name);
        if error_code == code {
            found = Some(shape);
        }
    }
    found
}

/// Whether a shape is an exception (`shape.metadata['exception']`).
pub fn is_exception(shape: &Shape<'_>) -> bool {
    shape
        .shape_get("exception")
        .and_then(|e| e.as_bool())
        .unwrap_or(false)
}

/// `http.client.responses` for the statuses `_do_generic_error_parse` can
/// see (500 and above).
fn http_reason(status: u16) -> &'static str {
    match status {
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        506 => "Variant Also Negotiates",
        507 => "Insufficient Storage",
        508 => "Loop Detected",
        510 => "Not Extended",
        511 => "Network Authentication Required",
        _ => "",
    }
}

/// `ResponseParser._is_generic_error_response`.
pub fn is_generic_error_response(response: &HttpResponse) -> bool {
    if response.status < 500 {
        return false;
    }
    let body = response.body.trim_ascii();
    body.is_empty() || body.starts_with(b"<html>")
}

/// `ResponseParser._do_generic_error_parse`.
pub fn generic_error_parse(response: &HttpResponse) -> Map {
    let mut error = Map::new();
    error.insert("Code".into(), Value::str(response.status.to_string()));
    error.insert("Message".into(), Value::str(http_reason(response.status)));
    let mut parsed = Map::new();
    parsed.insert("Error".into(), Value::Map(error));
    parsed.insert("ResponseMetadata".into(), Value::map());
    parsed
}

/// The tail of `ResponseParser.parse`: `HTTPStatusCode` and `HTTPHeaders`
/// are set on `ResponseMetadata` (added last when the protocol did not
/// create it).
pub fn add_http_metadata(parsed: &mut Map, response: &HttpResponse, headers: &HeaderDict) {
    let entry = parsed
        .entry("ResponseMetadata".to_string())
        .or_insert_with(Value::map);
    if let Value::Map(m) = entry {
        m.insert(
            "HTTPStatusCode".into(),
            Value::int(i64::from(response.status)),
        );
        m.insert("HTTPHeaders".into(), headers.lowercase());
    }
}

/// The steps `botocore/endpoint.py` runs after the protocol parser:
/// `_add_modeled_error_fields` for statuses of 300 and above, then
/// `RetryAttempts` (0 here; the retry loop owns the real count).
pub fn finish_parse<F>(
    model: &ServiceModel,
    response: &HttpResponse,
    mut parsed: Map,
    parse_with_shape: F,
) -> Result<Value>
where
    F: Fn(Option<Shape<'_>>) -> Result<Map>,
{
    if response.status >= 300 {
        let code = parsed
            .get("Error")
            .and_then(|e| e.get("Code"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(code) = code
            && let Some(shape) = shape_for_error_code(model, &code)
        {
            let modeled = parse_with_shape(Some(shape))?;
            for (k, v) in modeled {
                parsed.insert(k, v);
            }
        }
    }
    if let Some(Value::Map(m)) = parsed.get_mut("ResponseMetadata") {
        m.insert("RetryAttempts".into(), Value::int(0));
    }
    Ok(Value::Map(parsed))
}

/// Build a [`crate::protocol::ServiceError`] from a parsed error response.
pub fn service_error(parsed: Value) -> crate::protocol::ServiceError {
    let field = |key: &str| -> String {
        match parsed.get("Error").and_then(|e| e.get(key)) {
            Some(Value::Str(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => py_str(v).unwrap_or_default(),
        }
    };
    crate::protocol::ServiceError {
        code: field("Code"),
        message: field("Message"),
        parsed,
    }
}

#[cfg(test)]
pub mod testutil {
    //! A canonical dump of [`Value`]s that keeps key order and Python
    //! types, matching `dump()` in the golden generator script.

    use crate::value::Value;

    pub fn dump(v: &Value) -> String {
        let mut out = String::new();
        dump_into(&mut out, v);
        out
    }

    fn dump_into(out: &mut String, v: &Value) {
        match v {
            Value::Null => out.push_str("None"),
            Value::Bool(true) => out.push_str("True"),
            Value::Bool(false) => out.push_str("False"),
            Value::Int(i) => out.push_str(&i.to_string()),
            Value::Float(f) => out.push_str(&format!("f{:016x}", f.to_bits())),
            Value::Str(s) => {
                out.push('s');
                out.push_str(&super::json_dumps(&Value::str(s.clone()), false).unwrap());
            }
            Value::Bytes(b) => {
                out.push_str("b'");
                out.push_str(&hex::encode(b));
                out.push('\'');
            }
            Value::List(l) => {
                out.push('[');
                for (i, x) in l.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    dump_into(out, x);
                }
                out.push(']');
            }
            Value::Map(m) => {
                out.push('{');
                for (i, (k, x)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    dump_into(out, &Value::str(k.clone()));
                    out.push_str(": ");
                    dump_into(out, x);
                }
                out.push('}');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (1.0, "1.0"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (123456789012345.6, "123456789012345.6"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (-0.0, "-0.0"),
            (0.0, "0.0"),
            (1700000000.5, "1700000000.5"),
            (2.5e-7, "2.5e-07"),
            (1e100, "1e+100"),
            (9999999999999998.0, "9999999999999998.0"),
        ];
        for (f, want) in cases {
            assert_eq!(py_float_repr(f), want, "{f}");
        }
    }

    #[test]
    fn json_dumps_matches_python() {
        let v =
            Value::parse_json(r#"{"a": "café 😀\n\u0001", "b": [1, 2.0, null, true], "c": {}}"#)
                .unwrap();
        assert_eq!(
            json_dumps(&v, false).unwrap(),
            r#"{"a": "caf\u00e9 \ud83d\ude00\n\u0001", "b": [1, 2.0, null, true], "c": {}}"#
        );
        assert_eq!(
            json_dumps(&v, true).unwrap(),
            r#"{"a":"caf\u00e9 \ud83d\ude00\n\u0001","b":[1,2.0,null,true],"c":{}}"#
        );
    }

    #[test]
    fn percent_encode_matches_quote() {
        assert_eq!(percent_encode("a b/c~d", ""), "a%20b%2Fc~d");
        assert_eq!(percent_encode("a b/c~d", "/~"), "a%20b/c~d");
        assert_eq!(percent_encode("é:*", ""), "%C3%A9%3A%2A");
    }

    #[test]
    fn python_numbers() {
        assert_eq!(py_int(&Value::str(" 1_000 ")).unwrap(), Value::int(1000));
        assert!(py_int(&Value::str("1.5")).is_err());
        assert_eq!(py_int(&Value::Float(-2.7)).unwrap(), Value::int(-2));
        assert_eq!(py_float(&Value::str("1e3")).unwrap(), Value::Float(1000.0));
        assert_eq!(py_float(&Value::int(3)).unwrap(), Value::Float(3.0));
        assert!(matches!(py_float(&Value::str("NaN")).unwrap(), Value::Float(f) if f.is_nan()));
        assert!(py_float(&Value::str("abc")).is_err());
        assert_eq!(b64decode("eyJhIjoxfQ==").unwrap(), br#"{"a":1}"#);
        assert!(b64decode("YQ").is_err());
    }

    /// `base64.b64decode` of CPython 3.14 (hex of the bytes, or the
    /// `binascii.Error`).
    #[test]
    fn b64decode_like_binascii() {
        let cases: &[(&str, Result<&str, &str>)] = &[
            (
                "a",
                Err(
                    "Invalid base64-encoded string: number of data characters (1) cannot be 1 more than a multiple of 4",
                ),
            ),
            ("aGk=", Ok("6869")),
            ("aGk", Err("Incorrect padding")),
            ("aGk==", Ok("6869")),
            (
                "a===",
                Err(
                    "Invalid base64-encoded string: number of data characters (1) cannot be 1 more than a multiple of 4",
                ),
            ),
            ("=aGk=", Ok("6869")),
            ("aG=k", Err("Incorrect padding")),
            ("aGk=junk", Err("Incorrect padding")),
            ("YQ", Err("Incorrect padding")),
            ("YQ=", Err("Incorrect padding")),
            ("YQ==", Ok("61")),
            ("Y=Q==", Ok("61")),
            (
                "abcde",
                Err(
                    "Invalid base64-encoded string: number of data characters (5) cannot be 1 more than a multiple of 4",
                ),
            ),
            ("aGVs bG8", Err("Incorrect padding")),
            ("!!aGk=", Ok("6869")),
            ("aGVsbA==", Ok("68656c6c")),
        ];
        for (input, want) in cases {
            let got = b64decode(input).map(hex::encode).map_err(|e| e.to_string());
            assert_eq!(got.as_deref().map_err(String::as_str), *want, "{input:?}");
        }
    }
}
