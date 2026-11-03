//! The `json` protocol: `botocore/serialize.py` `JSONSerializer` and
//! `botocore/parsers.py` `BaseJSONParser` / `JSONParser`. The JSON body
//! handlers here are shared with `rest-json`.

use anyhow::{Result, anyhow, bail};

use crate::model::{Operation, ServiceModel, Shape};
use crate::protocol::common::{
    self, HeaderDict, b64encode, event_stream_name, handle_float, is_json_value_header, py_float,
    py_int, value_bytes,
};
use crate::protocol::timestamp::{self, WireTimestamp};
use crate::protocol::{HttpRequest, HttpResponse, ParseOptions, ServiceError};
use crate::value::{Map, Value};

/// `JSONSerializer.serialize_to_request`.
pub fn serialize(
    model: &ServiceModel,
    operation: &Operation<'_>,
    params: &Value,
) -> Result<HttpRequest> {
    let target_prefix = model
        .metadata_str("targetPrefix")
        .ok_or_else(|| anyhow!("missing metadata.targetPrefix"))?;
    let json_version = model
        .metadata_str("jsonVersion")
        .ok_or_else(|| anyhow!("missing metadata.jsonVersion"))?;
    let mut headers = vec![
        (
            "X-Amz-Target".to_string(),
            format!("{target_prefix}.{}", operation.name),
        ),
        (
            "Content-Type".to_string(),
            format!("application/x-amz-json-{json_version}"),
        ),
    ];
    if common::is_query_compatible(model) {
        headers.push(("x-amzn-query-mode".into(), "true".into()));
    }
    let body = match operation.input() {
        Some(input) => serialize_body(params, &input)?,
        None => Value::map(),
    };
    Ok(HttpRequest {
        method: operation.http_method().to_string(),
        url_path: "/".into(),
        query: Vec::new(),
        headers,
        body: common::json_dumps(&body, false)?.into_bytes(),
        host_prefix: common::expand_host_prefix(operation, params)?,
    })
}

/// `JSONSerializer._serialize(body, params, shape)` at the top level (no
/// key): the members of a structure go straight into the body, and a
/// document structure lands under the `None` key, which `json.dumps`
/// writes as `"null"`.
pub fn serialize_body(params: &Value, shape: &Shape<'_>) -> Result<Value> {
    if shape.type_name() == "structure" && shape.is_document() {
        let mut m = Map::new();
        m.insert("null".into(), params.clone());
        return Ok(Value::Map(m));
    }
    serialize_value(params, shape)
}

/// `JSONSerializer._serialize` for one value.
pub fn serialize_value(value: &Value, shape: &Shape<'_>) -> Result<Value> {
    match shape.type_name() {
        "structure" => {
            if shape.is_document() {
                return Ok(value.clone());
            }
            let m = value
                .as_map()
                .ok_or_else(|| anyhow!("expected a structure for {}", shape.name))?;
            let mut out = Map::new();
            for (k, v) in m {
                let member = shape
                    .member_shape(k)
                    .ok_or_else(|| anyhow!("unknown member {k} of {}", shape.name))?;
                let key = member.get_str("locationName").unwrap_or(k);
                out.insert(key.to_string(), serialize_value(v, &member)?);
            }
            Ok(Value::Map(out))
        }
        "map" => {
            let m = value
                .as_map()
                .ok_or_else(|| anyhow!("expected a map for {}", shape.name))?;
            let value_shape = shape
                .map_value()
                .ok_or_else(|| anyhow!("map {} has no value shape", shape.name))?;
            let mut out = Map::new();
            for (k, v) in m {
                let item = if v == &Value::Null {
                    Value::Null
                } else {
                    serialize_value(v, &value_shape)?
                };
                out.insert(k.clone(), item);
            }
            Ok(Value::Map(out))
        }
        "list" => {
            let l = value
                .as_list()
                .ok_or_else(|| anyhow!("expected a list for {}", shape.name))?;
            let member = shape
                .list_member()
                .ok_or_else(|| anyhow!("list {} has no member shape", shape.name))?;
            l.iter()
                .map(|item| {
                    if item == &Value::Null {
                        Ok(Value::Null)
                    } else {
                        serialize_value(item, &member)
                    }
                })
                .collect::<Result<Vec<_>>>()
                .map(Value::List)
        }
        "timestamp" => timestamp::serialize(
            value,
            shape.get_str("timestampFormat").unwrap_or("unixtimestamp"),
        ),
        "blob" => Ok(Value::Str(b64encode(&value_bytes(value)?))),
        "float" | "double" => Ok(handle_float(value)),
        _ => Ok(value.clone()),
    }
}

/// The type handlers of `BaseJSONParser`, with the overrides
/// `RestJSONParser` / `BaseRestParser` add when `rest` is set.
pub struct JsonParser<'o> {
    pub rest: bool,
    pub options: &'o ParseOptions,
}

impl JsonParser<'_> {
    /// `ResponseParser._parse_shape`.
    pub fn parse_shape(&self, shape: &Shape<'_>, value: &Value) -> Result<Value> {
        match shape.type_name() {
            "structure" => self.handle_structure(shape, value),
            "map" => self.handle_map(shape, value),
            "list" => self.handle_list(shape, value),
            "timestamp" => {
                let wire = match value {
                    Value::Int(_) | Value::Float(_) => WireTimestamp::Number(value.clone()),
                    Value::Str(s) => WireTimestamp::Text(s.clone()),
                    other => bail!("Invalid timestamp \"{other:?}\""),
                };
                timestamp::try_output_value(wire, self.options.timestamp_format)
            }
            "boolean" if self.rest => Ok(Value::Bool(match value {
                Value::Bool(b) => *b,
                Value::Str(s) => s.to_lowercase() == "true",
                _ => false,
            })),
            "integer" | "long" if self.rest => py_int(value),
            "float" | "double" if self.rest => py_float(value),
            "string" if self.rest && is_json_value_header(shape) => {
                let text = value
                    .as_str()
                    .ok_or_else(|| anyhow!("expected a string header value"))?;
                let decoded = common::b64decode(text)?;
                let decoded = String::from_utf8(decoded)
                    .map_err(|e| anyhow!("jsonvalue header is not UTF-8: {e}"))?;
                Value::parse_json(&decoded).map_err(|e| anyhow!("jsonvalue header: {e}"))
            }
            _ => Ok(value.clone()),
        }
    }

    /// `ResponseParser._handle_list` (with `BaseRestParser._handle_list`
    /// splitting a header value on commas).
    fn handle_list(&self, shape: &Shape<'_>, value: &Value) -> Result<Value> {
        let split;
        let items = match value {
            Value::List(l) => l,
            Value::Str(s) if self.rest && shape.get_str("location") == Some("header") => {
                split = s
                    .split(',')
                    .map(|e| Value::str(e.trim()))
                    .collect::<Vec<_>>();
                &split
            }
            other => bail!("expected a list for {}, got {other:?}", shape.name),
        };
        let member = shape
            .list_member()
            .ok_or_else(|| anyhow!("list {} has no member shape", shape.name))?;
        items
            .iter()
            .map(|item| match item {
                Value::Null => Ok(Value::Null),
                item => self.parse_shape(&member, item),
            })
            .collect::<Result<Vec<_>>>()
            .map(Value::List)
    }

    /// `BaseJSONParser._handle_structure`.
    fn handle_structure(&self, shape: &Shape<'_>, value: &Value) -> Result<Value> {
        if shape.is_document() {
            return Ok(value.clone());
        }
        let m = match value {
            Value::Null => return Ok(Value::Null),
            Value::Map(m) => m,
            other => bail!("expected an object for {}, got {other:?}", shape.name),
        };
        let members = shape.members();
        if shape.is_union() {
            let set: Vec<&String> = m
                .iter()
                .filter(|(k, v)| k.as_str() != "__type" && **v != Value::Null)
                .map(|(k, _)| k)
                .collect();
            if set.len() != 1 {
                bail!(
                    "Invalid service response: {} must have one and only one member set.",
                    shape.name
                );
            }
            let tag = set[0];
            let known = members
                .iter()
                .any(|(n, s)| s.get_str("locationName").unwrap_or(n) == tag.as_str());
            if !known {
                let first = m.keys().next().cloned().unwrap_or_default();
                let mut name = Map::new();
                name.insert("name".into(), Value::Str(first));
                let mut out = Map::new();
                out.insert("SDK_UNKNOWN_MEMBER".into(), Value::Map(name));
                return Ok(Value::Map(out));
            }
        }
        let mut out = Map::new();
        for (name, member) in &members {
            let json_name = member.get_str("locationName").unwrap_or(name);
            if let Some(raw) = m.get(json_name)
                && raw != &Value::Null
            {
                out.insert(name.to_string(), self.parse_shape(member, raw)?);
            }
        }
        Ok(Value::Map(out))
    }

    /// `BaseJSONParser._handle_map` (null values kept).
    fn handle_map(&self, shape: &Shape<'_>, value: &Value) -> Result<Value> {
        let m = value
            .as_map()
            .ok_or_else(|| anyhow!("expected an object for {}", shape.name))?;
        let key_shape = shape
            .map_key()
            .ok_or_else(|| anyhow!("map {} has no key shape", shape.name))?;
        let value_shape = shape
            .map_value()
            .ok_or_else(|| anyhow!("map {} has no value shape", shape.name))?;
        let mut out = Map::new();
        for (k, v) in m {
            let key = match self.parse_shape(&key_shape, &Value::str(k.clone()))? {
                Value::Str(s) => s,
                other => common::py_str(&other)?,
            };
            let item = match v {
                Value::Null => Value::Null,
                v => self.parse_shape(&value_shape, v)?,
            };
            out.insert(key, item);
        }
        Ok(Value::Map(out))
    }
}

/// `BaseJSONParser._parse_body_as_json`: an empty body is `{}`; a body
/// that is not JSON becomes `{"message": <body>}`.
pub fn parse_body_as_json(body: &[u8]) -> Result<Value> {
    if body.is_empty() {
        return Ok(Value::map());
    }
    let text = std::str::from_utf8(body).map_err(|e| anyhow!("response body is not UTF-8: {e}"))?;
    Ok(match Value::parse_json(text) {
        Ok(v) => v,
        Err(_) => {
            let mut m = Map::new();
            m.insert("message".into(), Value::str(text));
            Value::Map(m)
        }
    })
}

/// `body.get(key)` on a parsed JSON body.
fn body_get<'a>(body: &'a Value, key: &str) -> Result<Option<&'a Value>> {
    match body {
        Value::Map(m) => Ok(m.get(key)),
        other => bail!("'{}' object has no attribute 'get'", py_type(other)),
    }
}

fn py_type(v: &Value) -> &'static str {
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

/// `body.get('message', body.get('Message', ''))`.
pub fn error_message(body: &Value) -> Result<Value> {
    Ok(match body_get(body, "message")? {
        Some(v) => v.clone(),
        None => body_get(body, "Message")?
            .cloned()
            .unwrap_or(Value::str("")),
    })
}

/// Strip `aws.protocoltests#` / `:http://...` decorations from an error
/// code (`code.split(':', 1)[0]`, then `code.rsplit('#', 1)[-1]`).
pub fn sanitize_code(code: &str) -> String {
    let code = code.split(':').next().unwrap_or(code);
    code.rsplit('#').next().unwrap_or(code).to_string()
}

/// `BaseJSONParser._do_error_parse`.
pub fn do_error_parse(response: &HttpResponse, headers: &HeaderDict) -> Result<Map> {
    let body = parse_body_as_json(&response.body)?;
    let mut error = Map::new();
    error.insert("Message".into(), error_message(&body)?);
    error.insert("Code".into(), Value::str(""));
    let query_error = headers.get("x-amzn-query-error").unwrap_or("");
    let components: Vec<&str> = query_error.split(';').collect();
    let mut code: Option<Value> = None;
    if components.len() == 2 && !components[0].is_empty() {
        code = Some(Value::str(components[0]));
        error.insert("Type".into(), Value::str(components[1]));
    }
    if code.is_none() {
        code = match body_get(&body, "__type")? {
            Some(v) => (v != &Value::Null).then(|| v.clone()),
            None => Some(Value::str(response.status.to_string())),
        };
    }
    if let Some(code) = code {
        let code = match code {
            Value::Str(s) => Value::Str(sanitize_code(&s)),
            other => bail!("argument of type '{}' is not iterable", py_type(&other)),
        };
        error.insert("Code".into(), code);
    }
    let mut parsed = Map::new();
    parsed.insert("Error".into(), Value::Map(error));
    parsed.insert("ResponseMetadata".into(), Value::map());
    inject_response_metadata(&mut parsed, headers);
    Ok(parsed)
}

/// `BaseJSONParser._inject_response_metadata`.
fn inject_response_metadata(parsed: &mut Map, headers: &HeaderDict) {
    if let Some(id) = headers.get("x-amzn-requestid") {
        let entry = parsed
            .entry("ResponseMetadata".to_string())
            .or_insert_with(Value::map);
        if let Value::Map(m) = entry {
            m.insert("RequestId".into(), Value::str(id));
        }
    }
}

/// `JSONParser._handle_json_body`.
fn handle_json_body(body: &[u8], shape: &Shape<'_>, options: &ParseOptions) -> Result<Value> {
    let parsed = parse_body_as_json(body)?;
    JsonParser {
        rest: false,
        options,
    }
    .parse_shape(shape, &parsed)
}

fn into_map(v: Value) -> Result<Map> {
    match v {
        Value::Map(m) => Ok(m),
        Value::Null => Ok(Map::new()),
        other => bail!("parsed response is not a dict: {other:?}"),
    }
}

/// `JSONParser.parse` (`ResponseParser.parse` with the JSON handlers).
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
            return into_map(handle_json_body(&response.body, &shape, options)?);
        } else {
            do_error_parse(response, headers)?
        }
    } else {
        let mut parsed = match &shape {
            // `JSONParser._handle_event_stream`: the initial response is
            // the body; the rest of the stream stays as bytes for
            // `protocol::eventstream`.
            Some(shape) => match event_stream_name(shape) {
                Some(name) => {
                    let (initial, rest) =
                        crate::protocol::eventstream::initial_response(&response.body)?;
                    let mut parsed = into_map(handle_json_body(&initial, shape, options)?)?;
                    parsed.insert(name.to_string(), Value::Bytes(rest));
                    parsed
                }
                None => into_map(handle_json_body(&response.body, shape, options)?)?,
            },
            None => Map::new(),
        };
        inject_response_metadata(&mut parsed, headers);
        parsed
    };
    common::add_http_metadata(&mut parsed, response, headers);
    Ok(parsed)
}

/// Parse a `json` response the way `Endpoint._do_get_response` does.
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

/// Parse a `json` error response.
pub fn parse_error(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<ServiceError> {
    parse(model, operation, response, options).map(common::service_error)
}
