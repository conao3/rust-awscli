//! `after-call` handlers that decode response members:
//! `botocore/handlers.py` `json_decode_policies`, `decode_console_output`,
//! `json_decode_template_body`; the CLI's
//! `awscli/customizations/datapipeline/translator.py` `api_to_definition`
//! and `awscli/customizations/dynamodb/paginatorfix.py`
//! `parse_last_evaluated_key_binary`.

use anyhow::Result;

use super::{PythonError, key_error, unquote};
use crate::model::Shape;
use crate::protocol::common::b64decode;
use crate::value::{Map, Value};

/// `json_decode_policies`: every `policyDocumentType` string in the
/// output becomes the JSON document it URL-encodes.
pub(super) fn json_decode_policies(parsed: &mut Value, output: Option<Shape<'_>>) {
    if let Some(shape) = output {
        decode_policy_types(parsed, &shape);
    }
}

/// `_decode_policy_types`.
fn decode_policy_types(parsed: &mut Value, shape: &Shape<'_>) {
    match shape.type_name() {
        "structure" => {
            let Some(map) = parsed.as_map_mut() else {
                return;
            };
            for (name, member) in shape.members() {
                let Some(value) = map.get_mut(name) else {
                    continue;
                };
                if member.type_name() == "string" && member.name == "policyDocumentType" {
                    *value = decode_quoted_jsondoc(value);
                } else {
                    decode_policy_types(value, &member);
                }
            }
        }
        "list" => {
            if let (Some(member), Value::List(items)) = (shape.list_member(), parsed) {
                for item in items {
                    decode_policy_types(item, &member);
                }
            }
        }
        _ => {}
    }
}

/// `decode_quoted_jsondoc`: `json.loads(unquote(value))`, or the value
/// unchanged when that fails.
fn decode_quoted_jsondoc(value: &Value) -> Value {
    match value {
        Value::Str(s) => Value::parse_json(&unquote(s)).unwrap_or_else(|_| value.clone()),
        other => other.clone(),
    }
}

/// `decode_console_output`: `Output` is base64 of the console text,
/// decoded as UTF-8 with replacement.
pub(super) fn decode_console_output(parsed: &mut Value) {
    let Some(Value::Str(output)) = parsed.as_map_mut().and_then(|m| m.get_mut("Output")) else {
        return;
    };
    // `bytes(output, 'latin-1')` fails for characters above U+00FF; the
    // other non-ASCII characters are outside the base64 alphabet.
    if output.chars().any(|c| c as u32 > 0xff) {
        return;
    }
    if let Ok(bytes) = b64decode(output) {
        *output = String::from_utf8_lossy(&bytes).into_owned();
    }
}

/// `json_decode_template_body`.
pub(super) fn json_decode_template_body(parsed: &mut Value) {
    if let Some(body) = parsed.as_map_mut().and_then(|m| m.get_mut("TemplateBody"))
        && let Value::Str(text) = body
        && let Ok(decoded) = Value::parse_json(text)
    {
        *body = decoded;
    }
}

/// `translator.api_to_definition`.
pub(super) fn api_to_definition(parsed: &mut Value) -> Result<()> {
    let Some(definition) = parsed.as_map_mut() else {
        return Ok(());
    };
    if let Some(objects) = definition.shift_remove("pipelineObjects") {
        let converted = api_to_objects_definition(&objects)?;
        definition.insert("objects".to_string(), converted);
    }
    if let Some(objects) = definition.shift_remove("parameterObjects") {
        let converted = api_to_parameters_definition(&objects)?;
        definition.insert("parameters".to_string(), converted);
    }
    if let Some(values) = definition.shift_remove("parameterValues") {
        let converted = api_to_values_definition(&values)?;
        definition.insert("values".to_string(), converted);
    }
    Ok(())
}

fn items(v: &Value) -> Result<&[Value]> {
    match v {
        Value::List(l) => Ok(l),
        other => {
            Err(PythonError(format!("'{}' object is not iterable", py_type_name(other))).into())
        }
    }
}

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

fn get<'a>(v: &'a Value, key: &str) -> Result<&'a Value> {
    v.get(key).ok_or_else(|| key_error(key))
}

/// `_api_to_objects_definition`.
fn api_to_objects_definition(api: &Value) -> Result<Value> {
    let mut out = Vec::new();
    for element in items(api)? {
        let mut current = Map::new();
        current.insert("id".to_string(), get(element, "id")?.clone());
        current.insert("name".to_string(), get(element, "name")?.clone());
        for field in items(get(element, "fields")?)? {
            let key = crate::pyfmt::py_str(get(field, "key")?);
            let value = match field.get("stringValue") {
                Some(v) => v.clone(),
                None => {
                    let mut r = Map::new();
                    r.insert("ref".to_string(), get(field, "refValue")?.clone());
                    Value::Map(r)
                }
            };
            add_value(key, value, &mut current);
        }
        out.push(Value::Map(current));
    }
    Ok(Value::List(out))
}

/// `_api_to_parameters_definition`.
fn api_to_parameters_definition(api: &Value) -> Result<Value> {
    let mut out = Vec::new();
    for element in items(api)? {
        let mut current = Map::new();
        current.insert("id".to_string(), get(element, "id")?.clone());
        for attribute in items(get(element, "attributes")?)? {
            let key = crate::pyfmt::py_str(get(attribute, "key")?);
            add_value(key, get(attribute, "stringValue")?.clone(), &mut current);
        }
        out.push(Value::Map(current));
    }
    Ok(Value::List(out))
}

/// `_api_to_values_definition`.
fn api_to_values_definition(api: &Value) -> Result<Value> {
    let mut values = Map::new();
    for element in items(api)? {
        let key = crate::pyfmt::py_str(get(element, "id")?);
        add_value(key, get(element, "stringValue")?.clone(), &mut values);
    }
    Ok(Value::Map(values))
}

/// `_add_value`: repeated keys aggregate into a list.
fn add_value(key: String, value: Value, current: &mut Map) {
    match current.get_mut(&key) {
        None => {
            current.insert(key, value);
        }
        Some(Value::List(list)) => list.push(value),
        Some(existing) => {
            let first = std::mem::take(existing);
            *existing = Value::List(vec![first, value]);
        }
    }
}

/// `parse_last_evaluated_key_binary`.
pub(super) fn parse_last_evaluated_key_binary(parsed: &mut Value) -> Result<()> {
    let Some(Value::Map(key)) = parsed
        .as_map_mut()
        .and_then(|m| m.get_mut("LastEvaluatedKey"))
    else {
        return Ok(());
    };
    for val in key.values_mut() {
        if let Some(Value::Str(b)) = val.as_map_mut().and_then(|m| m.get_mut("B")) {
            let decoded = b64decode(b).map_err(|e| PythonError(e.to_string()))?;
            *val.as_map_mut()
                .expect("checked")
                .get_mut("B")
                .expect("checked") = Value::Bytes(decoded);
        }
    }
    Ok(())
}
