//! The `rest-json` protocol: `botocore/serialize.py` `BaseRestSerializer`
//! and `RestJSONSerializer`, `botocore/parsers.py` `BaseRestParser` and
//! `RestJSONParser`.

use anyhow::{Result, anyhow};
use indexmap::IndexMap;

use crate::model::{Operation, ServiceModel, Shape};
use crate::protocol::common::{
    self, HeaderDict, b64encode, handle_float, is_json_value_header, percent_encode, py_str,
    value_bytes,
};
use crate::protocol::json::{self as json_proto, JsonParser};
use crate::protocol::timestamp;
use crate::protocol::{HttpRequest, HttpResponse, ParseOptions, ServiceError};
use crate::value::{Map, Value};

/// `BaseRestSerializer.QUERY_STRING_TIMESTAMP_FORMAT`.
const QUERY_STRING_TIMESTAMP_FORMAT: &str = "iso8601";
/// `BaseRestSerializer.HEADER_TIMESTAMP_FORMAT`.
const HEADER_TIMESTAMP_FORMAT: &str = "rfc822";

/// A query string value: one string or a list (expanded on encoding).
#[derive(Debug, Clone)]
pub enum QueryValue {
    One(Value),
    Many(Vec<Value>),
}

/// The location-partitioned input of `BaseRestSerializer` (shared by the
/// rest protocols).
#[derive(Debug, Default)]
pub struct Partitioned {
    pub uri_path_kwargs: IndexMap<String, Value>,
    pub query_string_kwargs: IndexMap<String, QueryValue>,
    pub body_kwargs: Map,
    pub headers: IndexMap<String, String>,
}

impl Partitioned {
    /// The query string pairs as `percent_encode_sequence` walks them
    /// (unencoded; list values expanded).
    pub fn query_pairs(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for (k, v) in &self.query_string_kwargs {
            match v {
                QueryValue::One(v) => out.push((k.clone(), py_str(v)?)),
                QueryValue::Many(items) => {
                    for item in items {
                        out.push((k.clone(), py_str(item)?));
                    }
                }
            }
        }
        Ok(out)
    }
}

/// `BaseRestSerializer._partition_parameters` over every non-`None`
/// parameter.
pub fn partition(params: &Value, shape: &Shape<'_>) -> Result<Partitioned> {
    let mut p = Partitioned::default();
    let Some(params) = params.as_map() else {
        return Ok(p);
    };
    for (name, value) in params {
        if value == &Value::Null {
            continue;
        }
        let member = shape
            .member_shape(name)
            .ok_or_else(|| anyhow!("unknown parameter {name}"))?;
        let key_name = member.get_str("locationName").unwrap_or(name).to_string();
        match member.get_str("location") {
            Some("uri") => {
                p.uri_path_kwargs
                    .insert(key_name, uri_and_query_string_value(value, &member)?);
            }
            Some("querystring") => {
                if let Value::Map(m) = value {
                    for (k, v) in m {
                        let qv = match v {
                            Value::List(l) => QueryValue::Many(l.clone()),
                            v => QueryValue::One(v.clone()),
                        };
                        p.query_string_kwargs.insert(k.clone(), qv);
                    }
                } else if member.type_name() == "list" {
                    let item_shape = member
                        .list_member()
                        .ok_or_else(|| anyhow!("list {} has no member", member.name))?;
                    let items = value
                        .as_list()
                        .ok_or_else(|| anyhow!("expected a list for {name}"))?
                        .iter()
                        .map(|v| uri_and_query_string_value(v, &item_shape))
                        .collect::<Result<Vec<_>>>()?;
                    p.query_string_kwargs
                        .insert(key_name, QueryValue::Many(items));
                } else {
                    p.query_string_kwargs.insert(
                        key_name,
                        QueryValue::One(uri_and_query_string_value(value, &member)?),
                    );
                }
            }
            Some("header") => {
                if member.type_name() == "list" && !value.is_truthy() {
                    continue;
                }
                p.headers
                    .insert(key_name, convert_header_value(&member, value)?);
            }
            Some("headers") => {
                let m = value
                    .as_map()
                    .ok_or_else(|| anyhow!("expected a map for {name}"))?;
                for (k, v) in m {
                    p.headers.insert(format!("{key_name}{k}"), py_str(v)?);
                }
            }
            _ => {
                p.body_kwargs.insert(name.clone(), value.clone());
            }
        }
    }
    Ok(p)
}

/// `BaseRestSerializer._get_uri_and_query_string_value`.
fn uri_and_query_string_value(value: &Value, member: &Shape<'_>) -> Result<Value> {
    Ok(match member.type_name() {
        "boolean" => Value::Str(py_str(value)?.to_lowercase()),
        "timestamp" => timestamp::serialize(
            value,
            member
                .get_str("timestampFormat")
                .unwrap_or(QUERY_STRING_TIMESTAMP_FORMAT),
        )?,
        "float" | "double" => Value::Str(py_str(&handle_float(value))?),
        _ => value.clone(),
    })
}

/// `BaseRestSerializer._convert_header_value`.
pub fn convert_header_value(shape: &Shape<'_>, value: &Value) -> Result<String> {
    match shape.type_name() {
        "timestamp" => {
            let secs = Value::int(timestamp::utc_epoch_seconds(value)?);
            py_str(&timestamp::serialize(
                &secs,
                shape
                    .get_str("timestampFormat")
                    .unwrap_or(HEADER_TIMESTAMP_FORMAT),
            )?)
        }
        "list" => {
            let member = shape
                .list_member()
                .ok_or_else(|| anyhow!("list {} has no member", shape.name))?;
            let items = value
                .as_list()
                .ok_or_else(|| anyhow!("expected a list for header {}", shape.name))?;
            let mut parts = Vec::new();
            for v in items.iter().filter(|v| **v != Value::Null) {
                if member.type_name() == "string" {
                    let s = v
                        .as_str()
                        .ok_or_else(|| anyhow!("expected a string in header list"))?;
                    parts.push(escape_header_list_string(s));
                } else {
                    parts.push(convert_header_value(&member, v)?);
                }
            }
            Ok(parts.join(","))
        }
        _ if is_json_value_header(shape) => {
            Ok(b64encode(common::json_dumps(value, true)?.as_bytes()))
        }
        "boolean" => Ok(py_str(value)?.to_lowercase()),
        "float" | "double" => py_str(&handle_float(value)),
        _ => py_str(value),
    }
}

/// `BaseRestSerializer._escape_header_list_string`.
fn escape_header_list_string(value: &str) -> String {
    if value.contains('"') || value.contains(',') {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

/// `BaseRestSerializer._render_uri_template`.
pub fn render_uri_template(template: &str, params: &IndexMap<String, Value>) -> Result<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after
            .find('}')
            .ok_or_else(|| anyhow!("unterminated label in {template}"))?;
        let label = &after[..end];
        let (name, greedy) = match label.strip_suffix('+') {
            Some(n) => (n, true),
            None => (label, false),
        };
        let value = params
            .get(name)
            .ok_or_else(|| anyhow!("missing URI label parameter {name}"))?;
        let text = py_str(value)?;
        out.push_str(&percent_encode(&text, if greedy { "/~" } else { "" }));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `RestJSONSerializer.serialize_to_request`.
pub fn serialize(
    _model: &ServiceModel,
    operation: &Operation<'_>,
    params: &Value,
) -> Result<HttpRequest> {
    let mut req = HttpRequest {
        method: operation.http_method().to_string(),
        host_prefix: common::expand_host_prefix(operation, params)?,
        ..HttpRequest::default()
    };
    let Some(shape) = operation.input() else {
        req.url_path = operation.request_uri().to_string();
        return Ok(req);
    };
    let partitioned = partition(params, &shape)?;
    req.url_path = render_uri_template(operation.request_uri(), &partitioned.uri_path_kwargs)?;
    req.query = partitioned.query_pairs()?;
    req.headers = partitioned
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let payload = shape.get_str("payload");
    let payload_shape = match payload {
        Some(p) => Some(
            shape
                .member_shape(p)
                .ok_or_else(|| anyhow!("payload member {p} not found"))?,
        ),
        None => None,
    };
    let streaming = payload_shape
        .as_ref()
        .is_some_and(|s| matches!(s.type_name(), "blob" | "string"));
    if streaming {
        req.body = match params.get(payload.unwrap_or_default()) {
            Some(v) if v != &Value::Null => value_bytes(v)?,
            _ => Vec::new(),
        };
    } else if let (Some(p), Some(ps)) = (payload, &payload_shape) {
        req.body = match params.get(p) {
            Some(v) if v != &Value::Null => body_params(v, ps)?,
            _ => b"{}".to_vec(),
        };
    } else if !partitioned.body_kwargs.is_empty() {
        req.body = body_params(&Value::Map(partitioned.body_kwargs.clone()), &shape)?;
    } else if requires_empty_body(&shape) {
        req.body = b"{}".to_vec();
    }

    if !streaming
        && !req.body.is_empty()
        && !req
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("Content-Type"))
    {
        req.headers
            .push(("Content-Type".into(), "application/json".into()));
    }
    Ok(req)
}

/// `RestJSONSerializer._serialize_body_params`.
fn body_params(params: &Value, shape: &Shape<'_>) -> Result<Vec<u8>> {
    let body = json_proto::serialize_body(params, shape)?;
    Ok(common::json_dumps(&body, false)?.into_bytes())
}

/// `RestJSONSerializer._requires_empty_body`.
fn requires_empty_body(shape: &Shape<'_>) -> bool {
    shape
        .members()
        .iter()
        .any(|(_, m)| m.get("location").is_none())
}

/// `BaseRestParser._populate_response_metadata`.
fn populate_response_metadata(headers: &HeaderDict) -> Value {
    let mut m = Map::new();
    if let Some(id) = headers.get("x-amzn-requestid") {
        m.insert("RequestId".into(), Value::str(id));
    } else if let Some(id) = headers.get("x-amz-request-id") {
        m.insert("RequestId".into(), Value::str(id));
        m.insert(
            "HostId".into(),
            Value::str(headers.get("x-amz-id-2").unwrap_or("")),
        );
    }
    Value::Map(m)
}

/// `BaseRestParser._add_modeled_parse`. A blob payload makes the operation
/// `has_streaming_output`, so the body arrives as a `StreamingBody` and
/// stays bytes; a string payload is the decoded body.
fn add_modeled_parse(
    response: &HttpResponse,
    headers: &HeaderDict,
    shape: &Shape<'_>,
    parser: &JsonParser<'_>,
    out: &mut Map,
) -> Result<()> {
    for (name, member) in shape.members() {
        match member.get_str("location") {
            Some("statusCode") => {
                let v = parser.parse_shape(&member, &Value::int(i64::from(response.status)))?;
                out.insert(name.to_string(), v);
            }
            Some("headers") => {
                let prefix = member.get_str("locationName").unwrap_or("").to_lowercase();
                let mut m = Map::new();
                for (k, v) in headers.iter() {
                    if k.to_lowercase().starts_with(&prefix) {
                        let name: String = k.chars().skip(prefix.chars().count()).collect();
                        m.insert(name, Value::str(v));
                    }
                }
                out.insert(name.to_string(), Value::Map(m));
            }
            Some("header") => {
                let header_name = member.get_str("locationName").unwrap_or(name);
                if let Some(v) = headers.get(header_name) {
                    let parsed = parser.parse_shape(&member, &Value::str(v))?;
                    out.insert(name.to_string(), parsed);
                }
            }
            _ => {}
        }
    }

    match shape.get_str("payload") {
        Some(payload) => {
            let body_shape = shape
                .member_shape(payload)
                .ok_or_else(|| anyhow!("payload member {payload} not found"))?;
            if body_shape.get_bool("eventstream") {
                // The raw event stream, read by `protocol::eventstream`.
                out.insert(payload.to_string(), Value::Bytes(response.body.clone()));
            } else if matches!(body_shape.type_name(), "string" | "blob") {
                let v = if body_shape.type_name() == "blob" {
                    Value::Bytes(response.body.clone())
                } else {
                    Value::Str(
                        String::from_utf8(response.body.clone())
                            .map_err(|e| anyhow!("response body is not UTF-8: {e}"))?,
                    )
                };
                out.insert(payload.to_string(), v);
            } else {
                let original = json_proto::parse_body_as_json(&response.body)?;
                out.insert(
                    payload.to_string(),
                    parser.parse_shape(&body_shape, &original)?,
                );
            }
        }
        None => {
            let original = json_proto::parse_body_as_json(&response.body)?;
            if let Value::Map(m) = parser.parse_shape(shape, &original)? {
                for (k, v) in m {
                    out.insert(k, v);
                }
            }
        }
    }
    Ok(())
}

/// `RestJSONParser._do_error_parse` (with `_inject_error_code`).
fn do_error_parse(response: &HttpResponse, headers: &HeaderDict) -> Result<Map> {
    let mut parsed = json_proto::do_error_parse(response, headers)?;
    let body = json_proto::parse_body_as_json(&response.body)?;
    let code = if let Some(c) = headers.get("x-amzn-errortype") {
        Some(Value::str(c))
    } else if let Value::Map(b) = &body
        && (b.contains_key("code") || b.contains_key("Code"))
    {
        Some(
            b.get("code")
                .or_else(|| b.get("Code"))
                .cloned()
                .unwrap_or(Value::str("")),
        )
    } else {
        None
    };
    if let Some(code) = code {
        let code = match code {
            Value::Str(s) => Value::Str(json_proto::sanitize_code(&s)),
            other => other,
        };
        if let Some(Value::Map(e)) = parsed.get_mut("Error") {
            e.insert("Code".into(), code);
        }
    }
    Ok(parsed)
}

/// `RestJSONParser.parse` (`ResponseParser.parse` with the rest handlers).
fn parse_with_shape(
    response: &HttpResponse,
    headers: &HeaderDict,
    shape: Option<Shape<'_>>,
    options: &ParseOptions,
) -> Result<Map> {
    let parser = JsonParser {
        rest: true,
        options,
    };
    let mut parsed = if response.status >= 301 {
        if common::is_generic_error_response(response) {
            common::generic_error_parse(response)
        } else if let Some(shape) = shape.filter(common::is_exception) {
            let mut out = Map::new();
            add_modeled_parse(response, headers, &shape, &parser, &mut out)?;
            return Ok(out);
        } else {
            do_error_parse(response, headers)?
        }
    } else {
        let mut out = Map::new();
        out.insert(
            "ResponseMetadata".into(),
            populate_response_metadata(headers),
        );
        if let Some(shape) = &shape {
            add_modeled_parse(response, headers, shape, &parser, &mut out)?;
        }
        out
    };
    common::add_http_metadata(&mut parsed, response, headers);
    Ok(parsed)
}

/// Parse a `rest-json` response the way `Endpoint._do_get_response` does.
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

/// Parse a `rest-json` error response.
pub fn parse_error(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<ServiceError> {
    parse(model, operation, response, options).map(common::service_error)
}
