//! Event stream responses: the binary message framing of
//! `botocore/eventstream.py` (`EventStreamBuffer`,
//! `EventStreamHeaderParser`, `EventStream`) and the event parsers of
//! `botocore/parsers.py` (`BaseEventStreamParser` with
//! `EventStreamJSONParser`, `EventStreamXMLParser`,
//! `EventStreamCBORParser`).

use anyhow::{Result, anyhow};
use indexmap::IndexMap;

use crate::model::{ServiceModel, Shape};
use crate::protocol::ParseOptions;
use crate::protocol::timestamp::{self, WireTimestamp};
use crate::value::{Map, Value};

const PRELUDE_LENGTH: usize = 12;
const MAX_HEADERS_LENGTH: usize = 128 * 1024;

/// A header value as `EventStreamHeaderParser` unpacks it.
#[derive(Debug, Clone, PartialEq)]
pub enum HeaderValue {
    Bool(bool),
    /// byte, short, integer, long, and timestamp (milliseconds).
    Int(i64),
    /// byte_array and uuid.
    Bytes(Vec<u8>),
    Str(String),
}

impl HeaderValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            HeaderValue::Str(s) => Some(s),
            _ => None,
        }
    }

    fn to_value(&self) -> Value {
        match self {
            HeaderValue::Bool(b) => Value::Bool(*b),
            HeaderValue::Int(i) => Value::int(*i),
            HeaderValue::Bytes(b) => Value::Bytes(b.clone()),
            HeaderValue::Str(s) => Value::str(s),
        }
    }
}

/// `EventStreamMessage`.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub headers: IndexMap<String, HeaderValue>,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(HeaderValue::as_str)
    }

    /// `to_response_dict`: error and exception messages are parsed as
    /// errors (status 400).
    pub fn is_error(&self) -> bool {
        matches!(
            self.header_str(":message-type"),
            Some("error") | Some("exception")
        )
    }
}

/// A `ParserError` of the decoder (its `str()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

/// `EventStreamBuffer`: messages are taken from the front of the data
/// added so far.
#[derive(Debug, Default)]
pub struct Buffer {
    data: Vec<u8>,
    /// `(total_length, headers_length)` of the validated prelude of the
    /// next message.
    prelude: Option<(usize, usize)>,
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn checksum(data: &[u8], expected: u32) -> Result<(), DecodeError> {
    let calculated = crc32fast::hash(data);
    if calculated != expected {
        return Err(DecodeError(format!(
            "Checksum mismatch: expected 0x{expected:08x}, calculated 0x{calculated:08x}"
        )));
    }
    Ok(())
}

impl Buffer {
    pub fn new() -> Buffer {
        Buffer::default()
    }

    /// The bytes not consumed by the messages taken so far.
    pub fn remaining(&self) -> usize {
        self.data.len()
    }

    pub fn add_data(&mut self, data: &[u8]) {
        self.data.extend_from_slice(data);
    }

    /// `EventStreamBuffer.next`: `None` when the data ends before the
    /// next message does.
    pub fn next_message(&mut self) -> Result<Option<Message>, DecodeError> {
        if self.data.len() < PRELUDE_LENGTH {
            return Ok(None);
        }
        let (total, headers_len) = match self.prelude {
            Some(p) => p,
            None => {
                let total = be32(&self.data[0..4]) as usize;
                let headers_len = be32(&self.data[4..8]) as usize;
                checksum(&self.data[0..8], be32(&self.data[8..12]))?;
                if headers_len > MAX_HEADERS_LENGTH {
                    return Err(DecodeError(format!(
                        "Header length of {headers_len} exceeded the maximum of {MAX_HEADERS_LENGTH}"
                    )));
                }
                self.prelude = Some((total, headers_len));
                (total, headers_len)
            }
        };
        if self.data.len() < total {
            return Ok(None);
        }
        let data = &self.data[..total];
        let payload_end = total.saturating_sub(4);
        let headers_end = (PRELUDE_LENGTH + headers_len).min(payload_end.max(PRELUDE_LENGTH));
        let crc_bytes = &data[payload_end.min(data.len())..];
        if crc_bytes.len() < 4 {
            return Err(DecodeError("unpack requires a buffer of 4 bytes".into()));
        }
        checksum(&data[..payload_end], be32(crc_bytes))?;
        let headers = parse_headers(&data[PRELUDE_LENGTH..headers_end])?;
        let payload = data[headers_end..payload_end].to_vec();
        self.data.drain(..total);
        self.prelude = None;
        Ok(Some(Message { headers, payload }))
    }
}

/// Every message of `data` (the buffer fed with the whole body).
pub fn decode_all(data: &[u8]) -> Result<Vec<Message>, DecodeError> {
    let mut buffer = Buffer::new();
    buffer.add_data(data);
    let mut out = Vec::new();
    while let Some(m) = buffer.next_message()? {
        out.push(m);
    }
    Ok(out)
}

fn utf8(b: &[u8]) -> Result<String, DecodeError> {
    String::from_utf8(b.to_vec()).map_err(|e| {
        let pos = e.utf8_error().valid_up_to();
        DecodeError(format!(
            "'utf-8' codec can't decode byte 0x{:02x} in position {pos}: invalid start byte",
            b[pos]
        ))
    })
}

/// `EventStreamHeaderParser.parse`.
fn parse_headers(mut data: &[u8]) -> Result<IndexMap<String, HeaderValue>, DecodeError> {
    let short = || DecodeError("unpack requires a buffer of more bytes".to_string());
    let mut headers = IndexMap::new();
    while !data.is_empty() {
        let n = data[0] as usize;
        let name = utf8(data.get(1..1 + n).ok_or_else(short)?)?;
        data = &data[1 + n..];
        let ty = *data.first().ok_or_else(short)?;
        data = &data[1..];
        let take = |data: &[u8], n: usize| data.get(..n).map(<[u8]>::to_vec).ok_or_else(short);
        let (value, used) = match ty {
            0 => (HeaderValue::Bool(true), 0),
            1 => (HeaderValue::Bool(false), 0),
            2 => (HeaderValue::Int(take(data, 1)?[0] as i8 as i64), 1),
            3 => {
                let b = take(data, 2)?;
                (HeaderValue::Int(i16::from_be_bytes([b[0], b[1]]) as i64), 2)
            }
            4 => {
                let b = take(data, 4)?;
                (
                    HeaderValue::Int(i32::from_be_bytes([b[0], b[1], b[2], b[3]]) as i64),
                    4,
                )
            }
            5 | 8 => {
                let b = take(data, 8)?;
                (
                    HeaderValue::Int(i64::from_be_bytes(b.try_into().expect("8 bytes"))),
                    8,
                )
            }
            6 | 7 => {
                let b = take(data, 2)?;
                let len = u16::from_be_bytes([b[0], b[1]]) as usize;
                let raw = data.get(2..2 + len).ok_or_else(short)?;
                let v = if ty == 6 {
                    HeaderValue::Bytes(raw.to_vec())
                } else {
                    HeaderValue::Str(utf8(raw)?)
                };
                (v, 2 + len)
            }
            9 => (HeaderValue::Bytes(take(data, 16)?), 16),
            other => return Err(DecodeError(other.to_string())),
        };
        data = &data[used..];
        if headers.contains_key(&name) {
            return Err(DecodeError(format!("Duplicate header present: \"{name}\"")));
        }
        headers.insert(name, value);
    }
    Ok(headers)
}

/// `EventStream.get_initial_response` for the RPC protocols: the payload
/// of the first message, which must be an `initial-response` event, and
/// the rest of the stream.
pub fn initial_response(body: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut buffer = Buffer::new();
    buffer.add_data(body);
    match buffer.next_message()? {
        Some(m) if m.header_str(":event-type") == Some("initial-response") => {
            let used = body.len() - buffer.remaining();
            Ok((m.payload, body[used..].to_vec()))
        }
        _ => Err(anyhow!("First event was not of type initial-response")),
    }
}

/// The protocol whose event parser reads the payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Json,
    Xml,
    Cbor,
}

impl Protocol {
    pub fn of(model: &ServiceModel) -> Protocol {
        match model.protocol() {
            "rest-xml" | "query" | "ec2" => Protocol::Xml,
            "smithy-rpc-v2-cbor" => Protocol::Cbor,
            _ => Protocol::Json,
        }
    }
}

/// What iterating an `EventStream` raises.
#[derive(Debug)]
pub enum EventError {
    /// `EventStreamError`: the parsed error (`{"Error": {"Code",
    /// "Message"}}`) of an error or exception message.
    Stream(Value),
    /// Any other exception (decoding, payload parsing).
    Other(anyhow::Error),
}

impl From<DecodeError> for EventError {
    fn from(e: DecodeError) -> EventError {
        EventError::Other(e.into())
    }
}

impl From<anyhow::Error> for EventError {
    fn from(e: anyhow::Error) -> EventError {
        EventError::Other(e)
    }
}

/// The event parser of one event stream shape.
pub struct EventParser<'m> {
    pub model: &'m ServiceModel,
    pub shape: Shape<'m>,
    pub protocol: Protocol,
    pub options: ParseOptions,
}

/// A body parsed by `_initial_body_parse`.
enum Body {
    Value(Value),
    Xml(crate::protocol::xml_common::Element),
}

impl EventParser<'_> {
    /// `EventStream._parse_event`: the parsed event, `None` for an event
    /// of a type the shape does not have (skipped by `__iter__`).
    pub fn parse(&self, message: &Message) -> Result<Option<Value>, EventError> {
        if message.is_error() {
            return Err(EventError::Stream(self.do_error_parse(message)?));
        }
        let Some(event_type) = message.header_str(":event-type") else {
            return Ok(None);
        };
        let Some(event_shape) = self.shape.member_shape(event_type) else {
            return Ok(None);
        };
        let mut out = Map::new();
        out.insert(
            event_type.to_string(),
            Value::Map(self.do_parse_event(message, &event_shape)?),
        );
        Ok(Some(Value::Map(out)))
    }

    /// `BaseEventStreamParser._do_error_parse`.
    fn do_error_parse(&self, message: &Message) -> Result<Value> {
        let header = |n: &str| message.header_str(n).unwrap_or("").to_string();
        let exception_shape = message
            .header_str(":exception-type")
            .and_then(|t| self.shape.member_shape(t));
        let (code, text) = match exception_shape {
            Some(shape) => {
                let body = self.initial_body_parse(&message.payload)?;
                let parsed = self.parse_shape(&shape, &body)?;
                let text = parsed
                    .get("Message")
                    .or_else(|| parsed.get("message"))
                    .cloned()
                    .unwrap_or_else(|| Value::str(""));
                (Value::str(header(":exception-type")), text)
            }
            None => (
                Value::str(header(":error-code")),
                Value::str(header(":error-message")),
            ),
        };
        let mut error = Map::new();
        error.insert("Code".to_string(), code);
        error.insert("Message".to_string(), text);
        let mut out = Map::new();
        out.insert("Error".to_string(), Value::Map(error));
        Ok(Value::Map(out))
    }

    /// `BaseEventStreamParser._do_parse` of an event structure:
    /// `_parse_non_payload_attrs` and `_parse_payload`.
    fn do_parse_event(&self, message: &Message, shape: &Shape<'_>) -> Result<Map> {
        let mut out = Map::new();
        let members = shape.members();
        for (name, member) in &members {
            if member.get_bool("eventheader")
                && let Some(value) = message.headers.get(*name)
            {
                let value = match (member.type_name(), value) {
                    ("timestamp", HeaderValue::Int(ms)) => timestamp::try_output_value(
                        WireTimestamp::Number(Value::Float(*ms as f64 / 1000.0)),
                        self.options.timestamp_format,
                    )?,
                    _ => value.to_value(),
                };
                out.insert(name.to_string(), value);
            }
        }
        if shape.get_bool("event") {
            for (name, member) in &members {
                if member.get_bool("eventpayload") {
                    let body = &message.payload;
                    let parsed = match member.type_name() {
                        "blob" => Value::Bytes(body.clone()),
                        "string" => Value::Str(
                            String::from_utf8(body.clone())
                                .map_err(|e| anyhow!("event payload is not UTF-8: {e}"))?,
                        ),
                        _ => {
                            let raw = self.initial_body_parse(body)?;
                            self.parse_shape(member, &raw)?
                        }
                    };
                    out.insert(name.to_string(), parsed);
                    return Ok(out);
                }
            }
            let raw = self.initial_body_parse(&message.payload)?;
            if let Value::Map(m) = self.parse_shape(shape, &raw)? {
                out.extend(m);
            }
        }
        Ok(out)
    }

    /// `_initial_body_parse` of the protocol's event parser.
    fn initial_body_parse(&self, body: &[u8]) -> Result<Body> {
        Ok(match self.protocol {
            Protocol::Json => Body::Value(crate::protocol::json::parse_body_as_json(body)?),
            Protocol::Xml => Body::Xml(if body.is_empty() {
                Default::default()
            } else {
                crate::protocol::xml_common::parse_xml(body)?
            }),
            Protocol::Cbor => Body::Value(if body.is_empty() {
                Value::map()
            } else {
                crate::protocol::cbor::decode_item(body, &self.options)?
            }),
        })
    }

    /// `_parse_shape` with the protocol's handlers.
    fn parse_shape(&self, shape: &Shape<'_>, body: &Body) -> Result<Value> {
        match body {
            Body::Value(v) => match self.protocol {
                Protocol::Cbor => crate::protocol::cbor::parse_shape(shape, v),
                _ => crate::protocol::json::JsonParser {
                    rest: false,
                    options: &self.options,
                }
                .parse_shape(shape, v),
            },
            Body::Xml(root) => {
                use crate::protocol::xml_common::{Flavor, Input, Node, XmlParser};
                XmlParser {
                    flavor: Flavor::RestXml,
                    model: self.model,
                    options: self.options,
                }
                .parse_shape(shape, Input::Node(Node::One(root)))
            }
        }
    }
}

/// `str(EventStreamError(parsed, operation_name))` (`ClientError`).
pub fn stream_error_message(parsed: &Value, operation_name: &str) -> String {
    let error = parsed.get("Error");
    let field = |k: &str| {
        error
            .and_then(|e| e.get(k))
            .and_then(Value::as_str)
            .unwrap_or("Unknown")
            .to_string()
    };
    format!(
        "An error occurred ({}) when calling the {operation_name} operation: {}",
        field("Code"),
        field("Message")
    )
}

/// The event stream member of an output shape.
pub fn stream_shape<'m>(output: &Shape<'m>) -> Option<(&'m str, Shape<'m>)> {
    output
        .members()
        .into_iter()
        .find(|(_, m)| m.get_bool("eventstream"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut h = Vec::new();
        for (k, v) in headers {
            h.push(k.len() as u8);
            h.extend_from_slice(k.as_bytes());
            h.push(7);
            h.extend_from_slice(&(v.len() as u16).to_be_bytes());
            h.extend_from_slice(v.as_bytes());
        }
        let total = 12 + h.len() + payload.len() + 4;
        let mut out = Vec::new();
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(&(h.len() as u32).to_be_bytes());
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(payload);
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }

    #[test]
    fn decodes_incrementally() {
        let a = encode(&[(":event-type", "Records")], b"abc");
        let b = encode(&[(":event-type", "End")], b"");
        let mut data = a.clone();
        data.extend_from_slice(&b);
        let mut buffer = Buffer::new();
        buffer.add_data(&data[..5]);
        assert_eq!(buffer.next_message().unwrap(), None);
        buffer.add_data(&data[5..a.len() + 3]);
        let m = buffer.next_message().unwrap().unwrap();
        assert_eq!(m.payload, b"abc");
        assert_eq!(buffer.next_message().unwrap(), None);
        buffer.add_data(&data[a.len() + 3..]);
        let m = buffer.next_message().unwrap().unwrap();
        assert_eq!(m.header_str(":event-type"), Some("End"));
    }

    #[test]
    fn checksum_errors_match_botocore() {
        let mut data = encode(&[(":event-type", "End")], b"x");
        let n = data.len();
        data[n - 5] ^= 1;
        let err = decode_all(&data).unwrap_err();
        assert!(err.0.starts_with("Checksum mismatch: expected 0x"), "{err}");
        let mut data = encode(&[(":event-type", "End")], b"x");
        data[0] ^= 1;
        assert!(
            decode_all(&data)
                .unwrap_err()
                .0
                .starts_with("Checksum mismatch")
        );
    }

    /// Expected values from `EventStreamJSONParser.parse` on these
    /// messages (`lambda` `InvokeWithResponseStream`).
    #[test]
    fn parses_rest_json_events_like_botocore() {
        let data = hex::decode("0000004400000031998f21da0d3a6d6573736167652d747970650700056576656e740b3a6576656e742d7479706507000c5061796c6f61644368756e6b6162632712b19c00000071000000331e4077000d3a6d6573736167652d747970650700056576656e740b3a6576656e742d7479706507000e496e766f6b65436f6d706c6574657b224572726f72436f6465223a2245222c224c6f67526573756c74223a226247396e222c224578747261223a317d8e8d265a000000730000004644efa1d30d3a6d6573736167652d74797065070009657863657074696f6e0f3a657863657074696f6e2d747970650700195265736f757263654e6f74466f756e64457863657074696f6e7b224d657373616765223a22676f6e65222c2254797065223a2278227d7811b394").unwrap();
        let model = ServiceModel::load("lambda").unwrap();
        let parser = EventParser {
            model: &model,
            shape: model
                .shape("InvokeWithResponseStreamResponseEvent")
                .unwrap(),
            protocol: Protocol::of(&model),
            options: Default::default(),
        };
        let messages = decode_all(&data).unwrap();
        let first = parser.parse(&messages[0]).unwrap().unwrap();
        assert_eq!(
            first.get("PayloadChunk").and_then(|c| c.get("Payload")),
            Some(&Value::Bytes(b"abc".to_vec()))
        );
        let second = parser.parse(&messages[1]).unwrap().unwrap();
        let complete = second.get("InvokeComplete").unwrap();
        assert_eq!(complete.get("ErrorCode"), Some(&Value::str("E")));
        assert_eq!(complete.get("LogResult"), Some(&Value::str("bG9n")));
        assert_eq!(complete.get("Extra"), None);
        match parser.parse(&messages[2]) {
            Err(EventError::Stream(parsed)) => {
                assert_eq!(
                    stream_error_message(&parsed, "InvokeWithResponseStream"),
                    "An error occurred () when calling the InvokeWithResponseStream operation: "
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn duplicate_header() {
        let data = encode(&[("a", "1"), ("a", "2")], b"");
        assert_eq!(
            decode_all(&data).unwrap_err().0,
            "Duplicate header present: \"a\""
        );
    }
}
