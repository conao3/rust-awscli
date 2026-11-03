//! Pieces shared by the query, ec2, and rest-xml protocols: an
//! `xml.etree.ElementTree`-compatible DOM and writer, the
//! `BaseXMLResponseParser` / `BaseRestParser` / `BaseRestSerializer` logic
//! of `botocore/parsers.py` and `botocore/serialize.py`, and the Python
//! value formatting those depend on (`str()`, `repr(float)`, `int()`,
//! `float()`, `urllib.parse.quote`).

use std::fmt::Write as _;

use anyhow::{Result, anyhow, bail};
use indexmap::IndexMap;
use num_bigint::BigInt;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde_json::Value as Json;

use crate::model::{ServiceModel, Shape};
use crate::protocol::timestamp::{self, WireTimestamp};
use crate::protocol::{HttpRequest, HttpResponse, ParseOptions, ServiceError};
use crate::value::{Map, Value};

// ---------------------------------------------------------------------------
// Python value formatting
// ---------------------------------------------------------------------------

/// Python `repr(float)`.
pub(crate) fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').expect("exponent");
    let exp: i32 = exp.parse().expect("exponent value");
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
        let n = digits.len() as i32;
        if exp >= n - 1 {
            out.push_str(&digits);
            out.extend(std::iter::repeat_n('0', (exp - (n - 1)) as usize));
            out.push_str(".0");
        } else if exp >= 0 {
            let point = (exp + 1) as usize;
            out.push_str(&digits[..point]);
            out.push('.');
            out.push_str(&digits[point..]);
        } else {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-exp - 1) as usize));
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let sign = if exp < 0 { '-' } else { '+' };
        let _ = write!(out, "e{sign}{:02}", exp.abs());
    }
    out
}

/// Python `repr(bytes)`.
pub(crate) fn bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from("b");
    out.push(quote);
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            c if c as char == quote => {
                out.push('\\');
                out.push(quote);
            }
            0x20..0x7f => out.push(c as char),
            c => {
                let _ = write!(out, "\\x{c:02x}");
            }
        }
    }
    out.push(quote);
    out
}

/// Python `str(value)` for the scalar values serializers stringify.
pub(crate) fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => float_repr(*f),
        Value::Str(s) => s.clone(),
        Value::Bytes(b) => bytes_repr(b),
        Value::List(l) => {
            let items: Vec<String> = l.iter().map(py_repr).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Map(m) => {
            let items: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{}: {}", py_repr(&Value::Str(k.clone())), py_repr(v)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

fn py_repr(v: &Value) -> String {
    match v {
        Value::Str(s) => {
            let quote = if s.contains('\'') && !s.contains('"') {
                '"'
            } else {
                '\''
            };
            let mut out = String::new();
            out.push(quote);
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\t' => out.push_str("\\t"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    c if c == quote => {
                        out.push('\\');
                        out.push(c);
                    }
                    c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                        let _ = write!(out, "\\x{:02x}", c as u32);
                    }
                    c => out.push(c),
                }
            }
            out.push(quote);
            out
        }
        other => py_str(other),
    }
}

/// Python `str.isspace()` for one character.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python `str.strip()`.
pub(crate) fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// Remove the underscores Python's `int()` / `float()` accept between
/// digits; `None` when an underscore is misplaced.
fn strip_digit_underscores(s: &str) -> Option<String> {
    if !s.contains('_') {
        return Some(s.to_string());
    }
    let chars: Vec<char> = s.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == '_' {
            let before = i > 0 && chars[i - 1].is_ascii_digit();
            let after = chars.get(i + 1).is_some_and(char::is_ascii_digit);
            if !before || !after {
                return None;
            }
        }
    }
    Some(chars.into_iter().filter(|c| *c != '_').collect())
}

/// Python `int(text)` for a base-10 string.
pub(crate) fn py_int(text: &str) -> Result<BigInt> {
    let err = || {
        anyhow!(
            "invalid literal for int() with base 10: {}",
            py_repr(&Value::str(text))
        )
    };
    let s = py_strip(text);
    let (sign, digits) = match s.as_bytes().first() {
        Some(b'+') => ("", &s[1..]),
        Some(b'-') => ("-", &s[1..]),
        _ => ("", s),
    };
    let digits = strip_digit_underscores(digits).ok_or_else(err)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(err());
    }
    format!("{sign}{digits}")
        .parse::<BigInt>()
        .map_err(|_| err())
}

/// Python `float(text)`.
pub(crate) fn py_float(text: &str) -> Result<f64> {
    let err = || {
        anyhow!(
            "could not convert string to float: {}",
            py_repr(&Value::str(text))
        )
    };
    let s = py_strip(text);
    let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
    let lower = unsigned.to_ascii_lowercase();
    if matches!(lower.as_str(), "inf" | "infinity" | "nan") {
        let neg = s.starts_with('-');
        let v = if lower == "nan" {
            f64::NAN
        } else {
            f64::INFINITY
        };
        return Ok(if neg { -v } else { v });
    }
    let cleaned = strip_digit_underscores(s).ok_or_else(err)?;
    let body = cleaned.strip_prefix(['+', '-']).unwrap_or(&cleaned);
    let valid = !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
        && body.bytes().any(|b| b.is_ascii_digit());
    if !valid {
        return Err(err());
    }
    cleaned.parse::<f64>().map_err(|_| err())
}

/// `urllib.parse.quote(value, safe=safe)` on UTF-8 bytes.
pub(crate) fn quote(value: &[u8], safe: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value {
        if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) || safe.as_bytes().contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// `urllib.parse.quote_plus(value, safe='')`.
pub(crate) fn quote_plus(value: &[u8]) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value {
        if b == b' ' {
            out.push('+');
        } else if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// `base64.b64encode(value).strip().decode()` where `value` is a `str`
/// (UTF-8 encoded first) or `bytes` (`Serializer._get_base64`).
pub(crate) fn b64encode(v: &Value) -> String {
    use base64::Engine as _;
    let bytes = match v {
        Value::Bytes(b) => b.clone(),
        other => py_str(other).into_bytes(),
    };
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// `base64.b64decode(text)` (non-strict `binascii.a2b_base64`).
pub(crate) fn b64decode(text: &str) -> Result<Vec<u8>> {
    if !text.is_ascii() {
        bail!("string argument should contain only ASCII characters");
    }
    let table = |c: u8| -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::new();
    let mut quad_pos = 0;
    let mut pads = 0;
    let mut left: u32 = 0;
    let mut data_chars = 0usize;
    for &c in text.as_bytes() {
        if c == b'=' {
            pads += 1;
            if quad_pos >= 2 && quad_pos + pads >= 4 {
                return Ok(out);
            }
            continue;
        }
        let Some(v) = table(c) else { continue };
        let v = v as u32;
        data_chars += 1;
        pads = 0;
        match quad_pos {
            0 => {
                quad_pos = 1;
                left = v;
            }
            1 => {
                quad_pos = 2;
                out.push(((left << 2) | (v >> 4)) as u8);
                left = v & 0x0f;
            }
            2 => {
                quad_pos = 3;
                out.push(((left << 4) | (v >> 2)) as u8);
                left = v & 0x03;
            }
            _ => {
                quad_pos = 0;
                out.push(((left << 6) | v) as u8);
                left = 0;
            }
        }
    }
    match quad_pos {
        0 => Ok(out),
        1 => bail!(
            "Invalid base64-encoded string: number of data characters ({data_chars}) cannot be 1 more than a multiple of 4"
        ),
        _ => bail!("Incorrect padding"),
    }
}

/// `json.dumps(value, separators=(',', ':'))` (ASCII-only output).
pub(crate) fn json_dumps_compact(v: &Value) -> String {
    let mut out = String::new();
    write_json(&mut out, v);
    out
}

fn write_json(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) if f.is_nan() => out.push_str("NaN"),
        Value::Float(f) if f.is_infinite() => {
            out.push_str(if *f > 0.0 { "Infinity" } else { "-Infinity" })
        }
        Value::Float(f) => out.push_str(&float_repr(*f)),
        Value::Str(s) => write_json_str(out, s),
        Value::Bytes(b) => write_json_str(out, &String::from_utf8_lossy(b)),
        Value::List(l) => {
            out.push('[');
            for (i, item) in l.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(out, item);
            }
            out.push(']');
        }
        Value::Map(m) => {
            out.push('{');
            for (i, (k, item)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_str(out, k);
                out.push(':');
                write_json(out, item);
            }
            out.push('}');
        }
    }
}

fn write_json_str(out: &mut String, s: &str) {
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
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `http.client.responses.get(status, '')`.
pub(crate) fn http_reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        208 => "Already Reported",
        226 => "IM Used",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a Teapot",
        421 => "Misdirected Request",
        422 => "Unprocessable Content",
        423 => "Locked",
        424 => "Failed Dependency",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
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

// ---------------------------------------------------------------------------
// XML DOM (`xml.etree.ElementTree` with the expat `XMLParser`)
// ---------------------------------------------------------------------------

/// An `ElementTree.Element`: the tag in `{uri}local` form, the attributes
/// (namespaced names in `{uri}local` form, `xmlns` declarations removed),
/// the text before the first child (`None` when there is none), and the
/// child elements.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Element {
    pub tag: String,
    pub attrib: Vec<(String, String)>,
    pub text: Option<String>,
    pub children: Vec<Element>,
}

/// `ResponseParserError`.
#[derive(Debug)]
pub(crate) struct ResponseParserError(pub String);

impl std::fmt::Display for ResponseParserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ResponseParserError {}

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

struct Frame {
    element: Element,
    text: String,
    has_child: bool,
    scope: Vec<(String, String)>,
}

fn position(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let before = &text.as_bytes()[..offset];
    let line = before.iter().filter(|b| **b == b'\n').count() + 1;
    let line_start = before
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |p| p + 1);
    let column = String::from_utf8_lossy(&before[line_start..])
        .chars()
        .count();
    (line, column)
}

fn normalize_eol(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn check_chars(s: &str) -> bool {
    s.chars().all(|c| {
        !((c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r'))
            && c != '\u{fffe}'
            && c != '\u{ffff}'
    })
}

fn resolve_entity(name: &str) -> std::result::Result<String, &'static str> {
    if let Some(num) = name.strip_prefix('#') {
        let code = if let Some(hex) = num.strip_prefix('x') {
            u32::from_str_radix(hex, 16)
        } else {
            num.parse::<u32>()
        }
        .map_err(|_| "not well-formed (invalid token)")?;
        let c = char::from_u32(code).ok_or("reference to invalid character number")?;
        if (code < 0x20 && !matches!(code, 0x9 | 0xa | 0xd)) || code == 0xfffe || code == 0xffff {
            return Err("reference to invalid character number");
        }
        return Ok(c.to_string());
    }
    match name {
        "amp" => Ok("&".into()),
        "lt" => Ok("<".into()),
        "gt" => Ok(">".into()),
        "quot" => Ok("\"".into()),
        "apos" => Ok("'".into()),
        _ => Err("undefined entity"),
    }
}

/// Unescape an attribute value with XML attribute-value normalization.
fn attr_value(raw: &str) -> std::result::Result<String, &'static str> {
    let raw = normalize_eol(raw);
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw.as_str();
    while let Some(pos) = rest.find(['&', '<', '\t', '\n']) {
        out.push_str(&rest[..pos]);
        match rest.as_bytes()[pos] {
            b'<' => return Err("not well-formed (invalid token)"),
            b'\t' | b'\n' => {
                out.push(' ');
                rest = &rest[pos + 1..];
            }
            _ => {
                let end = rest[pos..]
                    .find(';')
                    .ok_or("not well-formed (invalid token)")?;
                out.push_str(&resolve_entity(&rest[pos + 1..pos + end])?);
                rest = &rest[pos + end + 1..];
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn lookup_ns<'a>(
    stack: &'a [Frame],
    scope: &'a [(String, String)],
    prefix: &str,
) -> Option<&'a str> {
    if prefix == "xml" {
        return Some(XML_NS);
    }
    scope
        .iter()
        .rev()
        .chain(stack.iter().rev().flat_map(|f| f.scope.iter().rev()))
        .find(|(p, _)| p == prefix)
        .map(|(_, uri)| uri.as_str())
}

fn qualify(uri: Option<&str>, local: &str) -> String {
    match uri {
        Some(uri) if !uri.is_empty() => format!("{{{uri}}}{local}"),
        _ => local.to_string(),
    }
}

/// `BaseXMLResponseParser._parse_xml_string_to_dom`: parse a document with
/// ElementTree's expat-based `XMLParser` semantics.
pub(crate) fn parse_xml(body: &[u8]) -> std::result::Result<Element, ResponseParserError> {
    parse_xml_inner(body).map_err(|reason| {
        ResponseParserError(format!(
            "Unable to parse response ({reason}), invalid XML received. Further retries may succeed:\n{}",
            bytes_repr(body)
        ))
    })
}

fn parse_xml_inner(body: &[u8]) -> std::result::Result<Element, String> {
    let fail = |msg: &str, text: &str, offset: usize| {
        let (line, column) = position(text, offset);
        format!("{msg}: line {line}, column {column}")
    };
    let text = match std::str::from_utf8(body) {
        Ok(t) => t,
        Err(e) => {
            let valid = std::str::from_utf8(&body[..e.valid_up_to()]).unwrap_or_default();
            return Err(fail("not well-formed (invalid token)", valid, valid.len()));
        }
    };
    let bom = if text.starts_with('\u{feff}') { 3 } else { 0 };
    let doc = &text[bom..];
    let mut reader = Reader::from_str(doc);
    {
        let config = reader.config_mut();
        config.trim_text(false);
        config.expand_empty_elements = false;
        config.check_end_names = true;
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut root: Option<Element> = None;
    loop {
        let start = reader.buffer_position() as usize;
        let event = match reader.read_event() {
            Ok(e) => e,
            Err(quick_xml::Error::IllFormed(
                quick_xml::errors::IllFormedError::MismatchedEndTag { .. },
            )) => return Err(fail("mismatched tag", doc, start)),
            Err(_) => {
                let pos = reader.error_position() as usize;
                return Err(fail("not well-formed (invalid token)", doc, pos));
            }
        };
        let in_root = !stack.is_empty();
        match event {
            Event::Start(e) | Event::Empty(e) if root.is_some() => {
                let _ = e;
                return Err(fail("junk after document element", doc, start));
            }
            Event::Start(ref e) | Event::Empty(ref e) => {
                let is_empty = matches!(event, Event::Empty(_));
                let name = std::str::from_utf8(e.name().as_ref())
                    .map_err(|_| fail("not well-formed (invalid token)", doc, start))?
                    .to_string();
                let mut scope = Vec::new();
                let mut raw_attrs = Vec::new();
                for attr in e.attributes() {
                    let attr =
                        attr.map_err(|_| fail("not well-formed (invalid token)", doc, start))?;
                    let key = std::str::from_utf8(attr.key.as_ref())
                        .map_err(|_| fail("not well-formed (invalid token)", doc, start))?
                        .to_string();
                    let raw = std::str::from_utf8(&attr.value)
                        .map_err(|_| fail("not well-formed (invalid token)", doc, start))?;
                    let value = attr_value(raw).map_err(|m| fail(m, doc, start))?;
                    if key == "xmlns" {
                        scope.push((String::new(), value));
                    } else if let Some(prefix) = key.strip_prefix("xmlns:") {
                        scope.push((prefix.to_string(), value));
                    } else {
                        raw_attrs.push((key, value));
                    }
                }
                let resolve = |qname: &str, is_attr: bool| -> std::result::Result<String, String> {
                    match qname.split_once(':') {
                        Some((prefix, local)) => {
                            let uri = lookup_ns(&stack, &scope, prefix)
                                .ok_or_else(|| fail("unbound prefix", doc, start))?;
                            Ok(qualify(Some(uri), local))
                        }
                        None if is_attr => Ok(qname.to_string()),
                        None => Ok(qualify(lookup_ns(&stack, &scope, ""), qname)),
                    }
                };
                let tag = resolve(&name, false)?;
                let mut attrib: Vec<(String, String)> = Vec::new();
                for (k, v) in raw_attrs {
                    let k = resolve(&k, true)?;
                    if attrib.iter().any(|(ek, _)| *ek == k) {
                        return Err(fail("duplicate attribute", doc, start));
                    }
                    attrib.push((k, v));
                }
                if let Some(parent) = stack.last_mut() {
                    parent.has_child = true;
                }
                let element = Element {
                    tag,
                    attrib,
                    text: None,
                    children: Vec::new(),
                };
                let frame = Frame {
                    element,
                    text: String::new(),
                    has_child: false,
                    scope,
                };
                if is_empty {
                    close(frame, &mut stack, &mut root);
                } else {
                    stack.push(frame);
                }
            }
            Event::End(_) => {
                let frame = stack.pop().expect("checked end names");
                close(frame, &mut stack, &mut root);
            }
            Event::Text(t) => {
                let raw = t
                    .decode()
                    .map_err(|_| fail("not well-formed (invalid token)", doc, start))?;
                if !check_chars(&raw) {
                    return Err(fail("not well-formed (invalid token)", doc, start));
                }
                if in_root {
                    let frame = stack.last_mut().expect("in root");
                    if !frame.has_child {
                        frame.text.push_str(&normalize_eol(&raw));
                    }
                } else if !raw.chars().all(|c| matches!(c, ' ' | '\t' | '\n' | '\r')) {
                    let msg = if root.is_some() {
                        "junk after document element"
                    } else {
                        "syntax error"
                    };
                    return Err(fail(msg, doc, start));
                }
            }
            Event::CData(t) => {
                if !in_root {
                    return Err(fail("syntax error", doc, start));
                }
                let raw = t
                    .decode()
                    .map_err(|_| fail("not well-formed (invalid token)", doc, start))?;
                let frame = stack.last_mut().expect("in root");
                if !frame.has_child {
                    frame.text.push_str(&normalize_eol(&raw));
                }
            }
            Event::GeneralRef(r) => {
                if !in_root {
                    return Err(fail("syntax error", doc, start));
                }
                let name = r
                    .decode()
                    .map_err(|_| fail("not well-formed (invalid token)", doc, start))?;
                let value = resolve_entity(&name).map_err(|m| fail(m, doc, start))?;
                let frame = stack.last_mut().expect("in root");
                if !frame.has_child {
                    frame.text.push_str(&value);
                }
            }
            Event::Comment(_) | Event::PI(_) | Event::Decl(_) | Event::DocType(_) => {}
            Event::Eof => {
                if !stack.is_empty() || root.is_none() {
                    return Err(fail("no element found", doc, doc.len()));
                }
                return Ok(root.expect("root"));
            }
        }
    }
}

fn close(frame: Frame, stack: &mut [Frame], root: &mut Option<Element>) {
    let mut element = frame.element;
    if !frame.text.is_empty() {
        element.text = Some(frame.text);
    }
    match stack.last_mut() {
        Some(parent) => parent.element.children.push(element),
        None => *root = Some(element),
    }
}

/// `BaseXMLResponseParser._node_tag`: `re.sub('{.*}', '', tag)`.
pub(crate) fn node_tag(tag: &str) -> &str {
    strip_namespace(tag)
}

fn strip_namespace(s: &str) -> &str {
    match (s.find('{'), s.rfind('}')) {
        (Some(0), Some(end)) => &s[end + 1..],
        _ => s,
    }
}

/// `re.sub('{.*}', replacement, s)` for attribute names.
fn sub_namespace(s: &str, replacement: &str) -> String {
    match (s.find('{'), s.rfind('}')) {
        (Some(start), Some(end)) if start < end => {
            format!("{}{replacement}{}", &s[..start], &s[end + 1..])
        }
        _ => s.to_string(),
    }
}

/// A value of the dict built by `_build_name_to_xml_node`: one node, or
/// the list repeated tags are aggregated into.
#[derive(Debug, Clone)]
pub(crate) enum Node<'a> {
    One(&'a Element),
    Many(Vec<&'a Element>),
}

impl<'a> Node<'a> {
    fn first(&self) -> &'a Element {
        match self {
            Node::One(e) => e,
            Node::Many(v) => v[0],
        }
    }
}

/// `BaseXMLResponseParser._build_name_to_xml_node`.
pub(crate) fn build_name_to_xml_node(parent: &Element) -> IndexMap<String, Node<'_>> {
    let mut map: IndexMap<String, Node<'_>> = IndexMap::new();
    for item in &parent.children {
        let key = node_tag(&item.tag).to_string();
        match map.get_mut(&key) {
            Some(Node::Many(list)) => list.push(item),
            Some(existing) => {
                let first = existing.first();
                *existing = Node::Many(vec![first, item]);
            }
            None => {
                map.insert(key, Node::One(item));
            }
        }
    }
    map
}

/// `BaseXMLResponseParser._replace_nodes` applied to
/// `_build_name_to_xml_node(root)`.
pub(crate) fn replace_nodes(parsed: &IndexMap<String, Node<'_>>) -> Map {
    let mut out = Map::new();
    for (key, node) in parsed {
        let has_children = match node {
            Node::One(e) => !e.children.is_empty(),
            Node::Many(_) => true,
        };
        let value = if has_children {
            Value::Map(replace_nodes(&build_name_to_xml_node(node.first())))
        } else {
            match &node.first().text {
                Some(t) => Value::Str(t.clone()),
                None => Value::Null,
            }
        };
        out.insert(key.clone(), value);
    }
    out
}

// ---------------------------------------------------------------------------
// XML writer (`ElementTree.tostring(elem, encoding='utf-8')`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(crate) struct XmlNode {
    pub tag: String,
    pub attrib: Vec<(String, String)>,
    pub text: Option<String>,
    pub children: Vec<XmlNode>,
}

impl XmlNode {
    pub fn new(tag: &str) -> XmlNode {
        XmlNode {
            tag: tag.to_string(),
            ..XmlNode::default()
        }
    }

    /// `node.attrib[key] = value`.
    pub fn set_attr(&mut self, key: &str, value: String) {
        match self.attrib.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.attrib.push((key.to_string(), value)),
        }
    }

    /// `ElementTree.SubElement(self, tag)`; returns the new child.
    pub fn sub_element(&mut self, tag: &str) -> &mut XmlNode {
        self.children.push(XmlNode::new(tag));
        self.children.last_mut().expect("just pushed")
    }

    pub fn write(&self, out: &mut String) {
        out.push('<');
        out.push_str(&self.tag);
        for (k, v) in &self.attrib {
            out.push(' ');
            out.push_str(k);
            out.push_str("=\"");
            out.push_str(&escape_attrib(v));
            out.push('"');
        }
        let text = self.text.as_deref().unwrap_or("");
        if !text.is_empty() || !self.children.is_empty() {
            out.push('>');
            out.push_str(&escape_cdata(text));
            for child in &self.children {
                child.write(out);
            }
            out.push_str("</");
            out.push_str(&self.tag);
            out.push('>');
        } else {
            out.push_str(" />");
        }
    }
}

/// `xml.etree.ElementTree._escape_cdata`.
fn escape_cdata(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `xml.etree.ElementTree._escape_attrib`.
fn escape_attrib(s: &str) -> String {
    escape_cdata(s)
        .replace('"', "&quot;")
        .replace('\r', "&#13;")
        .replace('\n', "&#10;")
        .replace('\t', "&#09;")
}

// ---------------------------------------------------------------------------
// Serializer helpers (`botocore/serialize.py` `Serializer`)
// ---------------------------------------------------------------------------

/// `shape.serialization.get('name')` (the `locationName` trait).
pub(crate) fn serialized_name<'a>(shape: &Shape<'a>) -> Option<&'a str> {
    shape.get_str("locationName")
}

/// `Serializer._handle_float` followed by `str()`.
pub(crate) fn float_str(v: &Value) -> String {
    match v {
        Value::Float(f) if f.is_nan() => "NaN".into(),
        Value::Float(f) if *f == f64::INFINITY => "Infinity".into(),
        Value::Float(f) if *f == f64::NEG_INFINITY => "-Infinity".into(),
        other => py_str(other),
    }
}

/// `Serializer._convert_timestamp_to_str`.
pub(crate) fn timestamp_str(v: &Value, format: Option<&str>) -> Result<String> {
    Ok(py_str(&timestamp::serialize(
        v,
        format.unwrap_or("iso8601"),
    )?))
}

/// `Serializer._expand_host_prefix`.
pub(crate) fn expand_host_prefix(
    operation: &crate::model::Operation<'_>,
    params: &Value,
) -> Result<Option<String>> {
    let Some(prefix) = operation
        .get("endpoint")
        .and_then(|e| e.get("hostPrefix"))
        .and_then(Json::as_str)
    else {
        return Ok(None);
    };
    let Some(input) = operation.input() else {
        return Ok(Some(prefix.to_string()));
    };
    let mut out = prefix.to_string();
    for (name, member) in input.members() {
        if member.get_bool("hostLabel") {
            let value = params
                .get(name)
                .ok_or_else(|| anyhow!("missing host label parameter: {name}"))?;
            out = out.replace(&format!("{{{name}}}"), &py_str(value));
        }
    }
    Ok(Some(out))
}

/// A `dict` of query parameters whose values may be lists (expanded the
/// way `percent_encode_sequence` does).
#[derive(Debug, Default)]
struct QueryParams(IndexMap<String, Vec<String>>);

/// `BaseRestSerializer.serialize_to_request`, with the body produced by
/// `serialize_body` (`_serialize_body_params`).
pub(crate) fn rest_serialize(
    operation: &crate::model::Operation<'_>,
    params: &Value,
    serialize_body: &dyn Fn(&Map, &Shape<'_>) -> Result<Vec<u8>>,
) -> Result<HttpRequest> {
    let mut req = HttpRequest {
        method: operation.http_method().to_string(),
        url_path: "/".into(),
        ..HttpRequest::default()
    };
    req.host_prefix = expand_host_prefix(operation, params)?;
    let Some(shape) = operation.input() else {
        req.url_path = operation.request_uri().to_string();
        return Ok(req);
    };
    let empty = Map::new();
    let params = params.as_map().unwrap_or(&empty);
    let mut uri_kwargs: IndexMap<String, String> = IndexMap::new();
    let mut query = QueryParams::default();
    let mut body_kwargs = Map::new();
    let mut headers: IndexMap<String, String> = IndexMap::new();
    for (name, value) in params {
        if matches!(value, Value::Null) {
            continue;
        }
        let member = shape
            .member_shape(name)
            .ok_or_else(|| anyhow!("unknown parameter: {name}"))?;
        let location = member.get_str("location");
        let key_name = serialized_name(&member).unwrap_or(name).to_string();
        match location {
            Some("uri") => {
                uri_kwargs.insert(key_name, uri_query_value(value, &member)?);
            }
            Some("querystring") => {
                if let Value::Map(m) = value {
                    for (k, v) in m {
                        let values = match v {
                            Value::List(l) => l.iter().map(py_str).collect(),
                            other => vec![py_str(other)],
                        };
                        query.0.insert(k.clone(), values);
                    }
                } else if member.type_name() == "list" {
                    let item = member
                        .list_member()
                        .ok_or_else(|| anyhow!("list without member"))?;
                    let values = value
                        .as_list()
                        .map(|l| l.iter().map(|v| uri_query_value(v, &item)).collect())
                        .transpose()?
                        .unwrap_or_default();
                    query.0.insert(key_name, values);
                } else {
                    query
                        .0
                        .insert(key_name, vec![uri_query_value(value, &member)?]);
                }
            }
            Some("header") => {
                if member.type_name() == "list" && !value.is_truthy() {
                    continue;
                }
                headers.insert(key_name, header_value(&member, value)?);
            }
            Some("headers") => {
                if let Value::Map(m) = value {
                    for (k, v) in m {
                        headers.insert(format!("{key_name}{k}"), py_str(v));
                    }
                }
            }
            _ => {
                body_kwargs.insert(name.clone(), value.clone());
            }
        }
    }
    req.url_path = render_uri_template(operation.request_uri(), &uri_kwargs)?;
    for (k, values) in query.0 {
        for v in values {
            req.query.push((k.clone(), v));
        }
    }
    req.headers = headers.into_iter().collect();

    let payload = shape.get_str("payload");
    match payload {
        Some(p) if is_blob_or_string(&shape, p) => {
            req.body = match params.get(p) {
                None => Vec::new(),
                Some(Value::Bytes(b)) => b.clone(),
                Some(other) => py_str(other).into_bytes(),
            };
        }
        Some(p) => {
            if let Some(Value::Map(body)) = params.get(p) {
                let member = shape
                    .member_shape(p)
                    .ok_or_else(|| anyhow!("unknown payload member: {p}"))?;
                req.body = serialize_body(body, &member)?;
            }
        }
        None if !body_kwargs.is_empty() => {
            req.body = serialize_body(&body_kwargs, &shape)?;
        }
        None => {}
    }
    Ok(req)
}

fn is_blob_or_string(shape: &Shape<'_>, payload: &str) -> bool {
    shape
        .member_shape(payload)
        .is_some_and(|m| matches!(m.type_name(), "blob" | "string"))
}

/// `BaseRestSerializer._get_uri_and_query_string_value` followed by
/// `str()`.
fn uri_query_value(value: &Value, member: &Shape<'_>) -> Result<String> {
    Ok(match member.type_name() {
        "boolean" => py_str(value).to_lowercase(),
        "timestamp" => timestamp_str(
            value,
            Some(member.get_str("timestampFormat").unwrap_or("iso8601")),
        )?,
        "float" | "double" => float_str(value),
        _ => py_str(value),
    })
}

/// `BaseRestSerializer._render_uri_template`.
fn render_uri_template(template: &str, params: &IndexMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        out.push_str(&rest[..start]);
        let name = &rest[start + 1..start + len];
        let (key, safe) = match name.strip_suffix('+') {
            Some(k) => (k, "/~"),
            None => (name, "-._~"),
        };
        let value = params
            .get(key)
            .ok_or_else(|| anyhow!("missing URI parameter: {key}"))?;
        out.push_str(&quote(value.as_bytes(), safe));
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `BaseRestSerializer._convert_header_value`.
fn header_value(shape: &Shape<'_>, value: &Value) -> Result<String> {
    match shape.type_name() {
        "timestamp" => {
            let epoch = match timestamp::serialize(value, "unixTimestamp")? {
                Value::Float(f) => Value::Int(BigInt::from(f.floor() as i64)),
                other => other,
            };
            timestamp_str(
                &epoch,
                Some(shape.get_str("timestampFormat").unwrap_or("rfc822")),
            )
        }
        "list" => {
            let member = shape
                .list_member()
                .ok_or_else(|| anyhow!("list without member"))?;
            let items = value.as_list().cloned().unwrap_or_default();
            let mut converted = Vec::new();
            for v in items.iter().filter(|v| !matches!(v, Value::Null)) {
                if member.type_name() == "string" {
                    let s = py_str(v);
                    converted.push(if s.contains('"') || s.contains(',') {
                        format!("\"{}\"", s.replace('"', "\\\""))
                    } else {
                        s
                    });
                } else {
                    converted.push(header_value(&member, v)?);
                }
            }
            Ok(converted.join(","))
        }
        "string" if is_json_value_header(shape) => {
            Ok(b64encode(&Value::Str(json_dumps_compact(value))))
        }
        "boolean" => Ok(py_str(value).to_lowercase()),
        "float" | "double" => Ok(float_str(value)),
        _ => Ok(py_str(value)),
    }
}

/// `botocore.utils.is_json_value_header`.
fn is_json_value_header(shape: &Shape<'_>) -> bool {
    shape.get_bool("jsonvalue")
        && shape.get_str("location") == Some("header")
        && shape.type_name() == "string"
}

// ---------------------------------------------------------------------------
// Response parsing (`botocore/parsers.py`)
// ---------------------------------------------------------------------------

/// Which concrete parser class is being ported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    /// `QueryParser`.
    Query,
    /// `EC2QueryParser`.
    Ec2,
    /// `RestXMLParser`.
    RestXml,
}

/// urllib3's `HTTPHeaderDict` as botocore sees it: case-insensitive keys
/// iterated in first-seen order with their original case, duplicate
/// values joined with `", "`.
pub(crate) struct Headers(Vec<(String, String)>);

impl Headers {
    pub fn new(raw: &[(String, String)]) -> Headers {
        let mut out: Vec<(String, String)> = Vec::new();
        for (k, v) in raw {
            match out.iter_mut().find(|(ek, _)| ek.eq_ignore_ascii_case(k)) {
                Some(slot) => {
                    slot.1.push_str(", ");
                    slot.1.push_str(v);
                }
                None => out.push((k.clone(), v.clone())),
            }
        }
        Headers(out)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `lowercase_dict(headers)`.
    pub fn lowercased(&self) -> Map {
        self.0
            .iter()
            .map(|(k, v)| (k.to_lowercase(), Value::Str(v.clone())))
            .collect()
    }
}

/// What a `_handle_*` method receives: an XML node (or the list of nodes
/// repeated tags became), header text, or the HTTP status code.
#[derive(Debug, Clone)]
pub(crate) enum Input<'a> {
    Node(Node<'a>),
    Text(String),
    Status(u16),
    Texts(Vec<String>),
}

pub(crate) struct XmlParser<'m> {
    pub flavor: Flavor,
    pub model: &'m ServiceModel,
    pub options: ParseOptions,
}

fn is_exception(shape: &Shape<'_>) -> bool {
    shape
        .shape_get("exception")
        .and_then(Json::as_bool)
        .unwrap_or(false)
}

impl XmlParser<'_> {
    /// `ResponseParser._parse_shape`.
    pub fn parse_shape(&self, shape: &Shape<'_>, input: Input<'_>) -> Result<Value> {
        match shape.type_name() {
            "structure" => self.handle_structure(shape, input),
            "list" => self.handle_list(shape, input),
            "map" => self.handle_map(shape, input),
            other => {
                let text = match input {
                    Input::Node(Node::One(e)) => e.text.clone().unwrap_or_default(),
                    Input::Node(Node::Many(_)) | Input::Texts(_) => {
                        bail!("cannot parse repeated elements as a {other}")
                    }
                    Input::Text(t) => t,
                    Input::Status(s) => s.to_string(),
                };
                self.handle_scalar(shape, other, text)
            }
        }
    }

    fn handle_scalar(&self, shape: &Shape<'_>, type_name: &str, text: String) -> Result<Value> {
        Ok(match type_name {
            "boolean" => Value::Bool(text == "true"),
            "float" | "double" => Value::Float(py_float(&text)?),
            "integer" | "long" => Value::Int(py_int(&text)?),
            "timestamp" => {
                timestamp::output_value(WireTimestamp::Text(text), self.options.timestamp_format)
            }
            "string" | "character"
                if self.flavor == Flavor::RestXml && is_json_value_header(shape) =>
            {
                let decoded = b64decode(&text)?;
                let decoded = String::from_utf8(decoded)
                    .map_err(|e| anyhow!("jsonvalue header is not UTF-8: {e}"))?;
                Value::parse_json(&decoded)?
            }
            // The CLI registers an identity blob parser
            // (`awscli/customizations/binaryformat.py`), so blobs stay base64
            // text.
            _ => Value::Str(text),
        })
    }

    /// `BaseXMLResponseParser._handle_structure`.
    fn handle_structure(&self, shape: &Shape<'_>, input: Input<'_>) -> Result<Value> {
        let Input::Node(node) = input else {
            bail!("cannot parse {} from a non-XML value", shape.name);
        };
        let mut node = node.first();
        if is_exception(shape) {
            node = self.get_error_root(node);
        }
        let xml_dict = build_name_to_xml_node(node);
        if shape.is_union() {
            let tags: Vec<&String> = xml_dict.keys().filter(|k| *k != "__type").collect();
            if tags.len() != 1 {
                bail!(
                    "Invalid service response: {} must have one and only one member set.",
                    shape.name
                );
            }
            let tag = tags[0];
            let known = shape
                .members()
                .iter()
                .any(|(name, m)| serialized_name(m).unwrap_or(name) == tag);
            if !known {
                let first = xml_dict.keys().next().expect("non-empty").clone();
                let mut inner = Map::new();
                inner.insert("name".into(), Value::Str(first));
                let mut out = Map::new();
                out.insert("SDK_UNKNOWN_MEMBER".into(), Value::Map(inner));
                return Ok(Value::Map(out));
            }
        }
        let mut parsed = Map::new();
        for (member_name, member) in shape.members() {
            if member.get("location").is_some() || member.get_bool("eventheader") {
                continue;
            }
            let xml_name = member_key_name(&member, member_name);
            if let Some(member_node) = xml_dict.get(xml_name) {
                let value = self.parse_shape(&member, Input::Node(member_node.clone()))?;
                parsed.insert(member_name.to_string(), value);
            } else if member.get_bool("xmlAttribute") {
                let location_name = serialized_name(&member).unwrap_or(member_name);
                let prefix = format!("{}:", location_name.split(':').next().unwrap_or(""));
                for (key, value) in &node.attrib {
                    if sub_namespace(key, &prefix) == location_name {
                        parsed.insert(member_name.to_string(), Value::Str(value.clone()));
                    }
                }
            }
        }
        Ok(Value::Map(parsed))
    }

    /// `_get_error_root` of `BaseXMLResponseParser` / `EC2QueryParser`.
    fn get_error_root<'e>(&self, root: &'e Element) -> &'e Element {
        if self.flavor == Flavor::Ec2 {
            for child in &root.children {
                if node_tag(&child.tag) == "Errors" {
                    for errors_child in &child.children {
                        if node_tag(&errors_child.tag) == "Error" {
                            return errors_child;
                        }
                    }
                }
            }
            return root;
        }
        if node_tag(&root.tag) == "ErrorResponse" {
            for child in &root.children {
                if node_tag(&child.tag) == "Error" {
                    return child;
                }
            }
        }
        root
    }

    /// `BaseRestParser._handle_list` / `BaseXMLResponseParser._handle_list`
    /// / `ResponseParser._handle_list`.
    fn handle_list(&self, shape: &Shape<'_>, input: Input<'_>) -> Result<Value> {
        let member = shape
            .list_member()
            .ok_or_else(|| anyhow!("list {} without member", shape.name))?;
        let input = match input {
            Input::Text(t)
                if self.flavor == Flavor::RestXml
                    && shape.get_str("location") == Some("header") =>
            {
                Input::Texts(t.split(',').map(|e| py_strip(e).to_string()).collect())
            }
            Input::Node(Node::One(e)) if shape.get_bool("flattened") => {
                Input::Node(Node::Many(vec![e]))
            }
            other => other,
        };
        let items: Vec<Input<'_>> = match input {
            Input::Node(Node::One(e)) => e
                .children
                .iter()
                .map(|c| Input::Node(Node::One(c)))
                .collect(),
            Input::Node(Node::Many(list)) => list
                .into_iter()
                .map(|c| Input::Node(Node::One(c)))
                .collect(),
            Input::Texts(texts) => texts.into_iter().map(Input::Text).collect(),
            Input::Text(t) => t.chars().map(|c| Input::Text(c.to_string())).collect(),
            Input::Status(_) => bail!("cannot parse a status code as a list"),
        };
        let mut parsed = Vec::with_capacity(items.len());
        for item in items {
            parsed.push(self.parse_shape(&member, item)?);
        }
        Ok(Value::List(parsed))
    }

    /// `BaseXMLResponseParser._handle_map`.
    fn handle_map(&self, shape: &Shape<'_>, input: Input<'_>) -> Result<Value> {
        let key_shape = shape.map_key().ok_or_else(|| anyhow!("map without key"))?;
        let value_shape = shape
            .map_value()
            .ok_or_else(|| anyhow!("map without value"))?;
        let key_location = serialized_name(&key_shape)
            .filter(|s| !s.is_empty())
            .unwrap_or("key");
        let value_location = serialized_name(&value_shape)
            .filter(|s| !s.is_empty())
            .unwrap_or("value");
        let Input::Node(node) = input else {
            bail!("cannot parse map {} from a non-XML value", shape.name);
        };
        let entries: Vec<&Element> = match node {
            Node::One(e) if shape.get_bool("flattened") => vec![e],
            Node::One(e) => e.children.iter().collect(),
            Node::Many(list) => list,
        };
        let mut parsed = Map::new();
        let mut key: Option<Value> = None;
        let mut val: Option<Value> = None;
        for entry in entries {
            for pair in &entry.children {
                let tag = node_tag(&pair.tag);
                if tag == key_location {
                    key = Some(self.parse_shape(&key_shape, Input::Node(Node::One(pair)))?);
                } else if tag == value_location {
                    val = Some(self.parse_shape(&value_shape, Input::Node(Node::One(pair)))?);
                } else {
                    bail!(ResponseParserError(format!("Unknown tag: {tag}")));
                }
            }
            let k = key
                .as_ref()
                .ok_or_else(|| anyhow!("map entry without a key"))?;
            let v = val
                .as_ref()
                .ok_or_else(|| anyhow!("map entry without a value"))?;
            let k = match k {
                Value::Str(s) => s.clone(),
                other => py_str(other),
            };
            parsed.insert(k, v.clone());
        }
        Ok(Value::Map(parsed))
    }

    // -- ResponseParser.parse ------------------------------------------------

    /// `ResponseParser.parse(response, shape)` followed by
    /// `Endpoint._add_modeled_error_fields` when the response is an error,
    /// and the `RetryAttempts` the endpoint sets after the first attempt.
    pub fn parse(
        &self,
        shape: Option<Shape<'_>>,
        response: &HttpResponse,
        error: bool,
    ) -> Result<Value> {
        let mut parsed = self.parse_one(shape, response, error)?;
        if error || response.status >= 300 {
            self.add_modeled_error_fields(&mut parsed, response)?;
        }
        if let Some(Value::Map(rm)) = parsed
            .as_map_mut()
            .and_then(|m| m.get_mut("ResponseMetadata"))
        {
            rm.insert("RetryAttempts".into(), Value::int(0));
        }
        Ok(parsed)
    }

    fn parse_one(
        &self,
        shape: Option<Shape<'_>>,
        response: &HttpResponse,
        error: bool,
    ) -> Result<Value> {
        let headers = Headers::new(&response.headers);
        let mut parsed = if error || response.status >= 301 {
            if is_generic_error_response(response) {
                generic_error(response)
            } else if shape.as_ref().is_some_and(is_exception) {
                return self.modeled_error_parse(shape.expect("checked"), response, &headers);
            } else {
                self.do_error_parse(response, &headers)?
            }
        } else {
            self.do_parse(shape, response, &headers)?
        };
        if let Value::Map(m) = &mut parsed {
            let mut rm = match m.get("ResponseMetadata") {
                Some(Value::Map(rm)) => rm.clone(),
                _ => Map::new(),
            };
            rm.insert("HTTPStatusCode".into(), Value::int(response.status as i64));
            rm.insert("HTTPHeaders".into(), Value::Map(headers.lowercased()));
            m.insert("ResponseMetadata".into(), Value::Map(rm));
        }
        Ok(parsed)
    }

    /// `Endpoint._add_modeled_error_fields`.
    fn add_modeled_error_fields(&self, parsed: &mut Value, response: &HttpResponse) -> Result<()> {
        let code = match parsed.get("Error").and_then(|e| e.get("Code")) {
            None | Some(Value::Null) => return Ok(()),
            Some(code) => py_str(code),
        };
        let Some(error_shape) = shape_for_error_code(self.model, &code) else {
            return Ok(());
        };
        let modeled = self.parse_one(Some(error_shape), response, true)?;
        if let (Some(target), Value::Map(fields)) = (parsed.as_map_mut(), modeled) {
            for (k, v) in fields {
                target.insert(k, v);
            }
        }
        Ok(())
    }

    fn do_parse(
        &self,
        shape: Option<Shape<'_>>,
        response: &HttpResponse,
        headers: &Headers,
    ) -> Result<Value> {
        match self.flavor {
            Flavor::Query | Flavor::Ec2 => self.parse_body_as_xml(shape, response, true),
            Flavor::RestXml => {
                let mut parsed = Map::new();
                parsed.insert(
                    "ResponseMetadata".into(),
                    Value::Map(populate_response_metadata(headers)),
                );
                if let Some(shape) = shape {
                    self.add_modeled_parse(&shape, response, headers, &mut parsed)?;
                }
                Ok(Value::Map(parsed))
            }
        }
    }

    fn modeled_error_parse(
        &self,
        shape: Shape<'_>,
        response: &HttpResponse,
        headers: &Headers,
    ) -> Result<Value> {
        match self.flavor {
            Flavor::Query | Flavor::Ec2 => self.parse_body_as_xml(Some(shape), response, false),
            Flavor::RestXml => {
                let mut parsed = Map::new();
                self.add_modeled_parse(&shape, response, headers, &mut parsed)?;
                Ok(Value::Map(parsed))
            }
        }
    }

    /// `QueryParser._parse_body_as_xml`.
    fn parse_body_as_xml(
        &self,
        shape: Option<Shape<'_>>,
        response: &HttpResponse,
        inject: bool,
    ) -> Result<Value> {
        let root = parse_xml(&response.body)?;
        let mut parsed = Value::map();
        if let Some(shape) = shape {
            let start = match shape.get_str("resultWrapper") {
                Some(wrapper) => build_name_to_xml_node(&root)
                    .get(wrapper)
                    .cloned()
                    .ok_or_else(|| anyhow!("KeyError: {}", py_repr(&Value::str(wrapper))))?,
                None => Node::One(&root),
            };
            parsed = self.parse_shape(&shape, Input::Node(start))?;
        }
        if inject && let Value::Map(m) = &mut parsed {
            let mapping = build_name_to_xml_node(&root);
            match self.flavor {
                Flavor::Ec2 => {
                    if let Some(child) = mapping.get("requestId") {
                        let mut rm = Map::new();
                        rm.insert("RequestId".into(), text_value(child.first()));
                        m.insert("ResponseMetadata".into(), Value::Map(rm));
                    }
                }
                _ => {
                    if let Some(child) = mapping.get("ResponseMetadata") {
                        let rm: Map = build_name_to_xml_node(child.first())
                            .iter()
                            .map(|(k, v)| (k.clone(), text_value(v.first())))
                            .collect();
                        m.insert("ResponseMetadata".into(), Value::Map(rm));
                    }
                }
            }
        }
        Ok(parsed)
    }

    /// `BaseRestParser._add_modeled_parse`.
    fn add_modeled_parse(
        &self,
        shape: &Shape<'_>,
        response: &HttpResponse,
        headers: &Headers,
        out: &mut Map,
    ) -> Result<()> {
        // _parse_non_payload_attrs
        for (name, member) in shape.members() {
            match member.get_str("location") {
                Some("statusCode") => {
                    let v = self.parse_shape(&member, Input::Status(response.status))?;
                    out.insert(name.to_string(), v);
                }
                Some("headers") => {
                    let prefix = serialized_name(&member).unwrap_or("").to_lowercase();
                    let mut map = Map::new();
                    for (k, v) in &headers.0 {
                        if k.to_lowercase().starts_with(&prefix) {
                            let key = k.get(prefix.len()..).unwrap_or("").to_string();
                            map.insert(key, Value::Str(v.clone()));
                        }
                    }
                    out.insert(name.to_string(), Value::Map(map));
                }
                Some("header") => {
                    let header_name = serialized_name(&member).unwrap_or(name);
                    if let Some(v) = headers.get(header_name) {
                        let v = self.parse_shape(&member, Input::Text(v.to_string()))?;
                        out.insert(name.to_string(), v);
                    }
                }
                _ => {}
            }
        }
        // _parse_payload
        if let Some(payload) = shape.get_str("payload") {
            let body_shape = shape
                .member_shape(payload)
                .ok_or_else(|| anyhow!("unknown payload member: {payload}"))?;
            if body_shape.get_bool("eventstream") {
                // The raw event stream; `commands::services::s3api`
                // decodes it (the only rest-xml event stream output is
                // `SelectObjectContent`).
                out.insert(payload.to_string(), Value::Bytes(response.body.clone()));
            } else if matches!(body_shape.type_name(), "string" | "blob") {
                let value =
                    if body_shape.is_streaming() {
                        Value::Bytes(response.body.clone())
                    } else {
                        Value::Str(String::from_utf8(response.body.clone()).map_err(|e| {
                            anyhow!("'utf-8' codec can't decode response body: {e}")
                        })?)
                    };
                out.insert(payload.to_string(), value);
            } else {
                let root = initial_body_parse(&response.body)?;
                let v = self.parse_shape(&body_shape, Input::Node(Node::One(&root)))?;
                out.insert(payload.to_string(), v);
            }
        } else {
            let root = initial_body_parse(&response.body)?;
            if let Value::Map(m) = self.parse_shape(shape, Input::Node(Node::One(&root)))? {
                for (k, v) in m {
                    out.insert(k, v);
                }
            }
        }
        Ok(())
    }

    fn do_error_parse(&self, response: &HttpResponse, headers: &Headers) -> Result<Value> {
        match self.flavor {
            Flavor::Query | Flavor::Ec2 => {
                let root = parse_xml(&response.body)?;
                let mut parsed = replace_nodes(&build_name_to_xml_node(&root));
                if let Some(errors) = parsed.shift_remove("Errors") {
                    match errors {
                        Value::Map(m) => {
                            for (k, v) in m {
                                parsed.insert(k, v);
                            }
                        }
                        _ => bail!("'NoneType' object is not iterable"),
                    }
                }
                if let Some(id) = parsed.shift_remove("RequestId") {
                    let mut rm = Map::new();
                    rm.insert("RequestId".into(), id);
                    parsed.insert("ResponseMetadata".into(), Value::Map(rm));
                }
                if self.flavor == Flavor::Ec2
                    && let Some(id) = parsed.shift_remove("RequestID")
                {
                    let mut rm = Map::new();
                    rm.insert("RequestId".into(), id);
                    parsed.insert("ResponseMetadata".into(), Value::Map(rm));
                }
                Ok(Value::Map(parsed))
            }
            Flavor::RestXml => {
                if !response.body.is_empty() {
                    match parse_error_from_body(response, headers) {
                        Ok(v) => return Ok(v),
                        Err(e) if e.is::<ResponseParserError>() => {}
                        Err(e) => return Err(e),
                    }
                }
                let mut error = Map::new();
                error.insert("Code".into(), Value::Str(response.status.to_string()));
                error.insert("Message".into(), Value::str(http_reason(response.status)));
                let mut rm = Map::new();
                rm.insert(
                    "RequestId".into(),
                    Value::str(headers.get("x-amz-request-id").unwrap_or("")),
                );
                rm.insert(
                    "HostId".into(),
                    Value::str(headers.get("x-amz-id-2").unwrap_or("")),
                );
                let mut out = Map::new();
                out.insert("Error".into(), Value::Map(error));
                out.insert("ResponseMetadata".into(), Value::Map(rm));
                Ok(Value::Map(out))
            }
        }
    }
}

fn text_value(e: &Element) -> Value {
    match &e.text {
        Some(t) => Value::Str(t.clone()),
        None => Value::Null,
    }
}

/// `BaseXMLResponseParser._member_key_name`.
fn member_key_name<'a>(shape: &Shape<'a>, member_name: &'a str) -> &'a str {
    if shape.type_name() == "list"
        && shape.get_bool("flattened")
        && let Some(name) = shape.list_member().as_ref().and_then(serialized_name)
    {
        return name;
    }
    serialized_name(shape).unwrap_or(member_name)
}

/// `RestXMLParser._initial_body_parse`.
fn initial_body_parse(body: &[u8]) -> Result<Element> {
    if body.is_empty() {
        return Ok(Element::default());
    }
    Ok(parse_xml(body)?)
}

/// `BaseRestParser._populate_response_metadata`.
fn populate_response_metadata(headers: &Headers) -> Map {
    let mut metadata = Map::new();
    if let Some(id) = headers.get("x-amzn-requestid") {
        metadata.insert("RequestId".into(), Value::str(id));
    } else if let Some(id) = headers.get("x-amz-request-id") {
        metadata.insert("RequestId".into(), Value::str(id));
        metadata.insert(
            "HostId".into(),
            Value::str(headers.get("x-amz-id-2").unwrap_or("")),
        );
    }
    metadata
}

/// `RestXMLParser._parse_error_from_body`.
fn parse_error_from_body(response: &HttpResponse, headers: &Headers) -> Result<Value> {
    let root = parse_xml(&response.body)?;
    let mut parsed = replace_nodes(&build_name_to_xml_node(&root));
    if root.tag == "Error" {
        let metadata = populate_response_metadata(headers);
        parsed.shift_remove("RequestId");
        parsed.shift_remove("HostId");
        let mut out = Map::new();
        out.insert("Error".into(), Value::Map(parsed));
        out.insert("ResponseMetadata".into(), Value::Map(metadata));
        return Ok(Value::Map(out));
    }
    if let Some(id) = parsed.shift_remove("RequestId") {
        let mut rm = Map::new();
        rm.insert("RequestId".into(), id);
        parsed.insert("ResponseMetadata".into(), Value::Map(rm));
    }
    let mut error = Map::new();
    error.insert("Message".into(), Value::str(""));
    error.insert("Code".into(), Value::str(""));
    let mut default = Map::new();
    default.insert("Error".into(), Value::Map(error));
    merge_dicts(&mut default, parsed);
    Ok(Value::Map(default))
}

/// `botocore.utils.merge_dicts` (without `append_lists`).
fn merge_dicts(dict1: &mut Map, dict2: Map) {
    for (key, value) in dict2 {
        match (dict1.get_mut(&key), value) {
            (Some(Value::Map(existing)), Value::Map(new)) => merge_dicts(existing, new),
            (_, value) => {
                dict1.insert(key, value);
            }
        }
    }
}

/// `ResponseParser._is_generic_error_response`.
fn is_generic_error_response(response: &HttpResponse) -> bool {
    if response.status < 500 {
        return false;
    }
    let body = response.body.trim_ascii();
    body.is_empty() || body.starts_with(b"<html>")
}

/// `ResponseParser._do_generic_error_parse`.
fn generic_error(response: &HttpResponse) -> Value {
    let mut error = Map::new();
    error.insert("Code".into(), Value::Str(response.status.to_string()));
    error.insert("Message".into(), Value::str(http_reason(response.status)));
    let mut out = Map::new();
    out.insert("Error".into(), Value::Map(error));
    out.insert("ResponseMetadata".into(), Value::map());
    Value::Map(out)
}

/// `ServiceModel.shape_for_error_code`: the last exception shape (in
/// model order) whose `error.code`, or name, is `code`.
fn shape_for_error_code<'m>(model: &'m ServiceModel, code: &str) -> Option<Shape<'m>> {
    let mut found = None;
    for name in model.shape_names() {
        let Some(shape) = model.shape(name) else {
            continue;
        };
        if !is_exception(&shape) {
            continue;
        }
        let shape_code = shape
            .shape_get("error")
            .and_then(|e| e.get("code"))
            .and_then(Json::as_str)
            .filter(|c| !c.is_empty())
            .unwrap_or(shape.name);
        if shape_code == code {
            found = Some(shape);
        }
    }
    found
}

/// Build the [`ServiceError`] from a parsed error response.
pub(crate) fn service_error(parsed: Value) -> ServiceError {
    let field = |key: &str| {
        parsed
            .get("Error")
            .and_then(|e| e.get(key))
            .map(|v| match v {
                Value::Null => String::new(),
                other => py_str(other),
            })
            .unwrap_or_default()
    };
    ServiceError {
        code: field("Code"),
        message: field("Message"),
        parsed,
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Golden comparison helpers shared by the protocol tests.

    use super::*;
    use crate::model::ServiceModel;

    /// A type-preserving dump matching the Python golden generator: `str`
    /// as `"s:<text>"`, `int` as `"i:<n>"`, `float` as `"f:<repr>"`,
    /// `bytes` as `"b:<hex>"`, `bool` / `None` / `list` / `dict` as JSON.
    pub fn canon(v: &Value) -> String {
        fn conv(v: &Value) -> Json {
            match v {
                Value::Null => Json::Null,
                Value::Bool(b) => Json::Bool(*b),
                Value::Int(i) => Json::String(format!("i:{i}")),
                Value::Float(f) => Json::String(format!("f:{}", float_repr(*f))),
                Value::Str(s) => Json::String(format!("s:{s}")),
                Value::Bytes(b) => Json::String(format!("b:{}", hex::encode(b))),
                Value::List(l) => Json::Array(l.iter().map(conv).collect()),
                Value::Map(m) => {
                    Json::Object(m.iter().map(|(k, v)| (k.clone(), conv(v))).collect())
                }
            }
        }
        serde_json::to_string(&conv(v)).unwrap()
    }

    /// The serialized request in the golden generator's JSON form.
    pub fn request_json(req: &HttpRequest) -> String {
        let pairs = |p: &[(String, String)]| {
            Json::Array(
                p.iter()
                    .map(|(k, v)| {
                        Json::Array(vec![Json::String(k.clone()), Json::String(v.clone())])
                    })
                    .collect(),
            )
        };
        let mut m = serde_json::Map::new();
        m.insert("method".into(), Json::String(req.method.clone()));
        m.insert("url_path".into(), Json::String(req.url_path.clone()));
        m.insert("query".into(), pairs(&req.query));
        m.insert("headers".into(), pairs(&req.headers));
        m.insert(
            "body".into(),
            Json::String(String::from_utf8(req.body.clone()).expect("UTF-8 body")),
        );
        m.insert(
            "host_prefix".into(),
            req.host_prefix.clone().map_or(Json::Null, Json::String),
        );
        serde_json::to_string(&Json::Object(m)).unwrap()
    }

    pub type Serializer =
        fn(&ServiceModel, &crate::model::Operation<'_>, &Value) -> Result<HttpRequest>;
    pub type Parser = fn(
        &ServiceModel,
        &crate::model::Operation<'_>,
        &HttpResponse,
        &ParseOptions,
    ) -> Result<Value>;

    pub fn check_serialize(f: Serializer, service: &str, op: &str, params: &str, expected: &str) {
        let model = ServiceModel::load(service).unwrap();
        let operation = model.operation(op).unwrap();
        let params = Value::parse_json(params).unwrap();
        let req = f(&model, &operation, &params).unwrap();
        assert_eq!(request_json(&req), expected, "{service} {op}");
    }

    pub fn response(status: u16, headers: &str, body: &str) -> HttpResponse {
        let headers: Vec<(String, String)> = serde_json::from_str(headers).unwrap();
        HttpResponse {
            status,
            headers,
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn check_parse(
        f: Parser,
        service: &str,
        op: &str,
        status: u16,
        headers: &str,
        body: &str,
        expected: &str,
    ) {
        let model = ServiceModel::load(service).unwrap();
        let operation = model.operation(op).unwrap();
        let resp = response(status, headers, body);
        let v = f(&model, &operation, &resp, &ParseOptions::default()).unwrap();
        assert_eq!(canon(&v), expected, "{service} {op}");
    }

    pub type ErrorParser = fn(
        &ServiceModel,
        &crate::model::Operation<'_>,
        &HttpResponse,
        &ParseOptions,
    ) -> Result<ServiceError>;

    pub fn check_parse_error(
        f: ErrorParser,
        service: &str,
        op: &str,
        status: u16,
        headers: &str,
        body: &str,
        expected: &str,
    ) {
        let model = ServiceModel::load(service).unwrap();
        let operation = model.operation(op).unwrap();
        let resp = response(status, headers, body);
        let e = f(&model, &operation, &resp, &ParseOptions::default()).unwrap();
        assert_eq!(canon(&e.parsed), expected, "{service} {op}");
        let expected: Json = serde_json::from_str(expected).unwrap();
        let field = |k: &str| {
            expected["Error"][k]
                .as_str()
                .map(|s| s.trim_start_matches("s:").to_string())
                .unwrap_or_default()
        };
        assert_eq!(e.code, field("Code"));
        assert_eq!(e.message, field("Message"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.5, "1.5"),
            (123.0, "123.0"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.2345e-7, "1.2345e-07"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e300, "1e+300"),
        ];
        for (f, s) in cases {
            assert_eq!(float_repr(f), s);
        }
    }

    #[test]
    fn python_number_parsing() {
        assert_eq!(py_int(" 42 ").unwrap(), BigInt::from(42));
        assert_eq!(py_int("-1_000").unwrap(), BigInt::from(-1000));
        assert_eq!(py_int("007").unwrap(), BigInt::from(7));
        assert!(py_int("1.0").is_err());
        assert!(py_int("_1").is_err());
        assert_eq!(py_float("1.5").unwrap(), 1.5);
        assert_eq!(py_float(" -Infinity ").unwrap(), f64::NEG_INFINITY);
        assert!(py_float("nan").unwrap().is_nan());
        assert_eq!(py_float("1e3").unwrap(), 1000.0);
        assert_eq!(py_float(".5").unwrap(), 0.5);
        assert!(py_float("abc").is_err());
    }

    #[test]
    fn b64decode_is_lenient_like_binascii() {
        assert_eq!(b64decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(b64decode("aGVs\nbG8=").unwrap(), b"hello");
        assert_eq!(b64decode("aGVsbG8=junk").unwrap(), b"hello");
        assert!(b64decode("aGVsbG8").is_err());
        assert!(b64decode("a").is_err());
    }

    #[test]
    fn parses_namespaces_like_elementtree() {
        let root = parse_xml(
            br#"<?xml version="1.0"?><a xmlns="urn:x" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="T" k="v&amp;
w"><b>1 &lt; 2<![CDATA[&]]></b><c/><d>x<e/>tail</d></a>"#,
        )
        .unwrap();
        assert_eq!(root.tag, "{urn:x}a");
        assert_eq!(
            root.attrib,
            vec![
                (
                    "{http://www.w3.org/2001/XMLSchema-instance}type".to_string(),
                    "T".to_string()
                ),
                ("k".to_string(), "v& w".to_string()),
            ]
        );
        assert_eq!(root.children[0].text.as_deref(), Some("1 < 2&"));
        assert_eq!(root.children[1].text, None);
        assert_eq!(root.children[2].text.as_deref(), Some("x"));
        assert_eq!(node_tag(&root.children[0].tag), "b");
    }

    #[test]
    fn rejects_non_xml() {
        for body in [
            &b""[..],
            b"Not Found",
            b"<a>",
            b"<a></b>",
            b"<a/><b/>",
            b"<p:a/>",
        ] {
            assert!(parse_xml(body).is_err(), "{body:?}");
        }
        let e = parse_xml(b"").unwrap_err();
        assert_eq!(
            e.0,
            "Unable to parse response (no element found: line 1, column 0), invalid XML received. Further retries may succeed:\nb''"
        );
    }

    #[test]
    fn writes_like_elementtree() {
        let mut root = XmlNode::new("R");
        root.set_attr("xmlns", "urn:x".into());
        root.sub_element("E");
        root.sub_element("T").text = Some("a<&>\"".into());
        root.sub_element("A").set_attr("k", "\"\n\t".into());
        let mut out = String::new();
        root.write(&mut out);
        assert_eq!(
            out,
            r#"<R xmlns="urn:x"><E /><T>a&lt;&amp;&gt;"</T><A k="&quot;&#10;&#09;" /></R>"#
        );
    }
}
