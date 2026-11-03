//! The `smithy-rpc-v2-cbor` protocol: `botocore/serialize.py`
//! `CBORSerializer` + `RpcV2CBORSerializer` and `botocore/parsers.py`
//! `BaseCBORParser` + `BaseRpcV2Parser` + `RpcV2CBORParser`.

use anyhow::{Result, anyhow, bail};
use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive};

use crate::model::{Operation, ServiceModel, Shape};
use crate::protocol::common::{self, HeaderDict, event_stream_name, value_bytes};
use crate::protocol::json::sanitize_code;
use crate::protocol::timestamp::{self, WireTimestamp};
use crate::protocol::{HttpRequest, HttpResponse, ParseOptions, ServiceError};
use crate::value::{Map, Value};

const UNSIGNED_INT: u8 = 0;
const NEGATIVE_INT: u8 = 1;
const BLOB: u8 = 2;
const STRING: u8 = 3;
const LIST: u8 = 4;
const MAP: u8 = 5;
const TAG: u8 = 6;
const FLOAT_AND_SIMPLE: u8 = 7;

/// `RpcV2CBORSerializer.serialize_to_request`.
pub fn serialize(
    model: &ServiceModel,
    operation: &Operation<'_>,
    params: &Value,
) -> Result<HttpRequest> {
    let target_prefix = model
        .metadata_str("targetPrefix")
        .ok_or_else(|| anyhow!("missing metadata.targetPrefix"))?;
    let mut body = Vec::new();
    if let Some(input) = operation.input() {
        serialize_data_item(&mut body, params, &input)?;
    }
    let host_prefix = common::expand_host_prefix(operation, params)?;
    let mut headers = vec![("smithy-protocol".to_string(), "rpc-v2-cbor".to_string())];
    let accept = if common::has_event_stream_output(operation) {
        "application/vnd.amazon.eventstream"
    } else {
        "application/cbor"
    };
    if common::is_query_compatible(model) {
        headers.push(("x-amzn-query-mode".into(), "true".into()));
    }
    headers.push(("Accept".into(), accept.into()));
    if !body.is_empty() {
        headers.push(("Content-Type".into(), accept.into()));
    }
    Ok(HttpRequest {
        method: "POST".into(),
        url_path: format!("/service/{target_prefix}/operation/{}", operation.name),
        query: Vec::new(),
        headers,
        body,
        host_prefix,
    })
}

/// `CBORSerializer._get_initial_byte` plus the length / value bytes chosen
/// by `_get_additional_info_and_num_bytes`.
fn write_head(out: &mut Vec<u8>, major: u8, value: u64) {
    let m = major << 5;
    if value < 24 {
        out.push(m | value as u8);
    } else if value < 256 {
        out.push(m | 24);
        out.push(value as u8);
    } else if value < 65_536 {
        out.push(m | 25);
        out.extend_from_slice(&(value as u16).to_be_bytes());
    } else if value < 4_294_967_296 {
        out.push(m | 26);
        out.extend_from_slice(&(value as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&value.to_be_bytes());
    }
}

fn write_null(out: &mut Vec<u8>) {
    out.push((FLOAT_AND_SIMPLE << 5) | 22);
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    write_head(out, STRING, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// `CBORSerializer._serialize_type_integer`.
fn write_integer(out: &mut Vec<u8>, value: &BigInt) -> Result<()> {
    let (major, n) = if value.is_negative() {
        (NEGATIVE_INT, -BigInt::from(1) - value)
    } else {
        (UNSIGNED_INT, value.clone())
    };
    let n = n
        .to_u64()
        .ok_or_else(|| anyhow!("int too big to convert"))?;
    write_head(out, major, n);
    Ok(())
}

fn as_f64(value: &Value) -> Result<f64> {
    match value {
        Value::Float(f) => Ok(*f),
        Value::Int(i) => i
            .to_f64()
            .ok_or_else(|| anyhow!("int too large to convert to float")),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        other => bail!("required argument is not a float: {other:?}"),
    }
}

/// `CBORSerializer._get_bytes_for_special_numbers`.
fn write_special(out: &mut Vec<u8>, f: f64) -> bool {
    let half: u16 = if f.is_nan() {
        0x7E00
    } else if f == f64::INFINITY {
        0x7C00
    } else if f == f64::NEG_INFINITY {
        0xFC00
    } else {
        return false;
    };
    out.push((FLOAT_AND_SIMPLE << 5) | 25);
    out.extend_from_slice(&half.to_be_bytes());
    true
}

/// `CBORSerializer._serialize_type_double`.
fn write_double(out: &mut Vec<u8>, f: f64) {
    if !write_special(out, f) {
        out.push((FLOAT_AND_SIMPLE << 5) | 27);
        out.extend_from_slice(&f.to_be_bytes());
    }
}

/// `CBORSerializer._serialize_data_item`.
fn serialize_data_item(out: &mut Vec<u8>, value: &Value, shape: &Shape<'_>) -> Result<()> {
    match shape.type_name() {
        "integer" | "long" => match value {
            Value::Int(i) => write_integer(out, i)?,
            Value::Bool(b) => write_integer(out, &BigInt::from(i64::from(*b)))?,
            other => bail!("expected an integer for {}, got {other:?}", shape.name),
        },
        "blob" => {
            let bytes = value_bytes(value)?;
            write_head(out, BLOB, bytes.len() as u64);
            out.extend_from_slice(&bytes);
        }
        "string" => {
            let s = value
                .as_str()
                .ok_or_else(|| anyhow!("expected a string for {}", shape.name))?;
            write_string(out, s);
        }
        "list" => {
            let l = value
                .as_list()
                .ok_or_else(|| anyhow!("expected a list for {}", shape.name))?;
            let member = shape
                .list_member()
                .ok_or_else(|| anyhow!("list {} has no member", shape.name))?;
            write_head(out, LIST, l.len() as u64);
            for item in l {
                if item == &Value::Null {
                    write_null(out);
                } else {
                    serialize_data_item(out, item, &member)?;
                }
            }
        }
        "map" => {
            let m = value
                .as_map()
                .ok_or_else(|| anyhow!("expected a map for {}", shape.name))?;
            let key_shape = shape
                .map_key()
                .ok_or_else(|| anyhow!("map {} has no key", shape.name))?;
            let value_shape = shape
                .map_value()
                .ok_or_else(|| anyhow!("map {} has no value", shape.name))?;
            write_head(out, MAP, m.len() as u64);
            for (k, v) in m {
                serialize_data_item(out, &Value::str(k.clone()), &key_shape)?;
                if v == &Value::Null {
                    write_null(out);
                } else {
                    serialize_data_item(out, v, &value_shape)?;
                }
            }
        }
        "structure" => {
            let m = value
                .as_map()
                .ok_or_else(|| anyhow!("expected a structure for {}", shape.name))?;
            let present: Vec<(&String, &Value)> =
                m.iter().filter(|(_, v)| **v != Value::Null).collect();
            write_head(out, MAP, present.len() as u64);
            for (k, v) in present {
                let member = shape
                    .member_shape(k)
                    .ok_or_else(|| anyhow!("unknown member {k} of {}", shape.name))?;
                write_string(out, member.get_str("locationName").unwrap_or(k));
                serialize_data_item(out, v, &member)?;
            }
        }
        "timestamp" => {
            out.push((TAG << 5) | 1);
            match timestamp::serialize(value, "unixtimestamp")? {
                Value::Float(f) => write_double(out, f),
                Value::Int(i) => write_integer(out, &i)?,
                other => bail!("unexpected timestamp value {other:?}"),
            }
        }
        "float" => {
            let f = as_f64(value)?;
            if !write_special(out, f) {
                let single = f as f32;
                if single.is_infinite() {
                    bail!("float too large to pack with f format");
                }
                out.push((FLOAT_AND_SIMPLE << 5) | 26);
                out.extend_from_slice(&single.to_be_bytes());
            }
        }
        "double" => write_double(out, as_f64(value)?),
        "boolean" => {
            out.push((FLOAT_AND_SIMPLE << 5) | if value.is_truthy() { 21 } else { 20 });
        }
        other => bail!("'RpcV2CBORSerializer' object has no attribute '_serialize_type_{other}'"),
    }
    Ok(())
}

/// A decoder over `BaseCBORParser.parse_data_item`.
struct Decoder<'a, 'o> {
    data: &'a [u8],
    pos: usize,
    options: &'o ParseOptions,
}

impl Decoder<'_, '_> {
    fn read(&mut self, n: usize) -> Result<&[u8]> {
        if self.pos + n > self.data.len() {
            bail!(
                "End of stream reached; this indicates a malformed CBOR response from the server or an issue in botocore"
            );
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn read_uint(&mut self, n: usize) -> Result<u64> {
        let bytes = self.read(n)?;
        Ok(bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
    }

    /// `_handle_break_code`.
    fn at_break(&mut self) -> bool {
        if self.data.get(self.pos) == Some(&0xFF) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// `_parse_unsigned_integer`.
    fn unsigned(&mut self, info: u8) -> Result<u64> {
        match info {
            0..=23 => Ok(u64::from(info)),
            24 => self.read_uint(1),
            25 => self.read_uint(2),
            26 => self.read_uint(4),
            27 => self.read_uint(8),
            _ => bail!(
                "Invalid CBOR integer returned from the service; unparsable additional info found for major type 0 or 1: {info}"
            ),
        }
    }

    fn byte_string(&mut self, info: u8) -> Result<Vec<u8>> {
        if info != 31 {
            let len = self.unsigned(info)? as usize;
            return Ok(self.read(len)?.to_vec());
        }
        let mut out = Vec::new();
        while !self.at_break() {
            let initial = self.read_uint(1)? as u8;
            let len = self.unsigned(initial & 0x1F)? as usize;
            out.extend_from_slice(self.read(len)?);
        }
        Ok(out)
    }

    fn item(&mut self) -> Result<Value> {
        let initial = self.read_uint(1)? as u8;
        let major = initial >> 5;
        let info = initial & 0x1F;
        match major {
            0 => Ok(Value::Int(BigInt::from(self.unsigned(info)?))),
            1 => Ok(Value::Int(
                BigInt::from(-1) - BigInt::from(self.unsigned(info)?),
            )),
            2 => Ok(Value::Bytes(self.byte_string(info)?)),
            3 => String::from_utf8(self.byte_string(info)?)
                .map(Value::Str)
                .map_err(|e| anyhow!("'utf-8' codec can't decode CBOR text: {e}")),
            4 => {
                let mut items = Vec::new();
                if info != 31 {
                    let len = self.unsigned(info)?;
                    for _ in 0..len {
                        items.push(self.item()?);
                    }
                } else {
                    while !self.at_break() {
                        items.push(self.item()?);
                    }
                }
                Ok(Value::List(items))
            }
            5 => {
                let mut m = Map::new();
                if info != 31 {
                    let len = self.unsigned(info)?;
                    for _ in 0..len {
                        self.key_value(&mut m)?;
                    }
                } else {
                    while !self.at_break() {
                        self.key_value(&mut m)?;
                    }
                }
                Ok(Value::Map(m))
            }
            6 => {
                let tag = self.unsigned(info)?;
                let value = self.item()?;
                if tag != 1 {
                    bail!("Found CBOR tag not supported by botocore: {tag}");
                }
                match value {
                    Value::Int(_) | Value::Float(_) => timestamp::try_output_value(
                        WireTimestamp::Number(value),
                        self.options.timestamp_format,
                    ),
                    other => bail!("Unable to parse datetime value: {other:?}"),
                }
            }
            _ => match info {
                20 => Ok(Value::Bool(false)),
                21 => Ok(Value::Bool(true)),
                22 | 23 => Ok(Value::Null),
                25 => {
                    let bits = self.read_uint(2)? as u16;
                    Ok(Value::Float(f16_to_f64(bits)))
                }
                26 => {
                    let bits = self.read_uint(4)? as u32;
                    Ok(Value::Float(f64::from(f32::from_bits(bits))))
                }
                27 => Ok(Value::Float(f64::from_bits(self.read_uint(8)?))),
                _ => bail!(
                    "Invalid additional info found for major type 7: {info}.  This indicates an unsupported simple type or an indefinite float value"
                ),
            },
        }
    }

    fn key_value(&mut self, m: &mut Map) -> Result<()> {
        let key = match self.item()? {
            Value::Str(s) => s,
            other => bail!("unsupported CBOR map key {other:?}"),
        };
        let value = self.item()?;
        m.insert(key, value);
        Ok(())
    }
}

/// `struct.unpack('>e', ...)`.
fn f16_to_f64(bits: u16) -> f64 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((bits >> 10) & 0x1F);
    let frac = f64::from(bits & 0x3FF);
    let mag = match exp {
        0 => frac * 2f64.powi(-24),
        31 => {
            if frac == 0.0 {
                f64::INFINITY
            } else {
                f64::NAN
            }
        }
        _ => (1.0 + frac / 1024.0) * 2f64.powi(exp - 15),
    };
    sign * mag
}

/// `RpcV2CBORParser._initial_body_parse`.
fn initial_body_parse(body: &[u8], options: &ParseOptions) -> Result<Value> {
    if body.is_empty() {
        return Ok(Value::Bytes(Vec::new()));
    }
    Decoder {
        data: body,
        pos: 0,
        options,
    }
    .item()
}

/// `parse_data_item` of a whole body (`EventStreamCBORParser`).
pub(crate) fn decode_item(body: &[u8], options: &ParseOptions) -> Result<Value> {
    Decoder {
        data: body,
        pos: 0,
        options,
    }
    .item()
}

/// `ResponseParser._parse_shape` with the RPCv2 handlers: structures and
/// lists are walked, everything else (maps included) is returned as
/// decoded.
pub(crate) fn parse_shape(shape: &Shape<'_>, value: &Value) -> Result<Value> {
    match shape.type_name() {
        "structure" => {
            let m = value
                .as_map()
                .ok_or_else(|| anyhow!("'{value:?}' object has no attribute 'get'"))?;
            if shape.is_union() {
                let set = m
                    .iter()
                    .filter(|(k, v)| k.as_str() != "__type" && **v != Value::Null)
                    .count();
                if set != 1 {
                    bail!(
                        "Invalid service response: {} must have one and only one member set.",
                        shape.name
                    );
                }
            }
            let mut out = Map::new();
            for (name, member) in shape.members() {
                if let Some(v) = m.get(name)
                    && v != &Value::Null
                {
                    out.insert(name.to_string(), parse_shape(&member, v)?);
                }
            }
            Ok(Value::Map(out))
        }
        "list" => {
            let l = value
                .as_list()
                .ok_or_else(|| anyhow!("expected a list for {}", shape.name))?;
            let member = shape
                .list_member()
                .ok_or_else(|| anyhow!("list {} has no member", shape.name))?;
            l.iter()
                .map(|item| match item {
                    Value::Null => Ok(Value::Null),
                    item => parse_shape(&member, item),
                })
                .collect::<Result<Vec<_>>>()
                .map(Value::List)
        }
        _ => Ok(value.clone()),
    }
}

/// `RpcV2CBORParser._do_error_parse`.
fn do_error_parse(
    response: &HttpResponse,
    headers: &HeaderDict,
    options: &ParseOptions,
) -> Result<Map> {
    let body = initial_body_parse(&response.body, options)?;
    let b = body
        .as_map()
        .ok_or_else(|| anyhow!("'bytes' object has no attribute 'get'"))?;
    let message = match b.get("message") {
        Some(v) => v.clone(),
        None => b.get("Message").cloned().unwrap_or(Value::str("")),
    };
    let mut error = Map::new();
    error.insert("Message".into(), message);
    error.insert("Code".into(), Value::str(""));
    let code = match b.get("__type") {
        Some(Value::Null) | None => Some(Value::str(response.status.to_string())),
        Some(v) => Some(v.clone()),
    };
    if let Some(code) = code {
        let mut code = match code {
            Value::Str(s) => sanitize_code(&s),
            other => bail!("argument of type '{other:?}' is not iterable"),
        };
        if let Some(query_error) = headers.get("x-amzn-query-error") {
            let parts: Vec<&str> = query_error.split(';').collect();
            if parts.len() == 2 && !parts[0].is_empty() {
                error.insert("QueryErrorCode".into(), Value::str(code));
                error.insert("Type".into(), Value::str(parts[1]));
                code = parts[0].to_string();
            }
        }
        error.insert("Code".into(), Value::Str(code));
    }
    let mut parsed = Map::new();
    parsed.insert("Error".into(), Value::Map(error));
    let mut metadata = Map::new();
    if let Some(id) = headers.get("x-amzn-requestid") {
        metadata.insert("RequestId".into(), Value::str(id));
    }
    parsed.insert("ResponseMetadata".into(), Value::Map(metadata));
    Ok(parsed)
}

/// `BaseRpcV2Parser._parse_payload` into `out`.
fn parse_payload(
    response: &HttpResponse,
    shape: &Shape<'_>,
    options: &ParseOptions,
    out: &mut Map,
) -> Result<()> {
    let original = initial_body_parse(&response.body, options)?;
    if let Value::Map(m) = parse_shape(shape, &original)? {
        for (k, v) in m {
            out.insert(k, v);
        }
    }
    Ok(())
}

/// `RpcV2CBORParser.parse` (`ResponseParser.parse`).
fn parse_with_shape(
    response: &HttpResponse,
    headers: &HeaderDict,
    shape: Option<Shape<'_>>,
    options: &ParseOptions,
) -> Result<Map> {
    let mut parsed = if response.status >= 301 {
        if common::is_generic_error_response(response) {
            common::generic_error_parse(response)
        } else if let Some(shape) = shape.filter(common::is_exception) {
            let mut out = Map::new();
            parse_payload(response, &shape, options, &mut out)?;
            return Ok(out);
        } else {
            do_error_parse(response, headers, options)?
        }
    } else {
        let mut out = Map::new();
        if let Some(shape) = &shape {
            match event_stream_name(shape) {
                // `RpcV2CBORParser._handle_event_stream`.
                Some(name) => {
                    let (initial, rest) =
                        crate::protocol::eventstream::initial_response(&response.body)?;
                    let initial = HttpResponse {
                        status: response.status,
                        headers: response.headers.clone(),
                        body: initial,
                    };
                    parse_payload(&initial, shape, options, &mut out)?;
                    out.insert(name.to_string(), Value::Bytes(rest));
                }
                None => parse_payload(response, shape, options, &mut out)?,
            }
            let mut metadata = Map::new();
            if let Some(id) = headers.get("x-amzn-requestid") {
                metadata.insert("RequestId".into(), Value::str(id));
            }
            out.insert("ResponseMetadata".into(), Value::Map(metadata));
        }
        out
    };
    common::add_http_metadata(&mut parsed, response, headers);
    Ok(parsed)
}

/// Parse a `smithy-rpc-v2-cbor` response the way
/// `Endpoint._do_get_response` does.
pub fn parse(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<Value> {
    let headers = HeaderDict::from_response(response);
    let parsed = parse_with_shape(response, &headers, operation.output(), options)?;
    common::finish_parse(model, response, parsed, |shape| {
        parse_with_shape(response, &headers, shape, options)
    })
}

/// Parse a `smithy-rpc-v2-cbor` error response.
pub fn parse_error(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<ServiceError> {
    parse(model, operation, response, options).map(common::service_error)
}
