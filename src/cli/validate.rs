//! Parameter validation against the input shape (`botocore/validate.py`
//! `ParamValidator`), with the report text botocore renders.

use serde_json::Value as Json;

use super::shorthand::py_type_name;
use crate::model::Shape;
use crate::pyfmt::{float_repr, py_str};
use crate::value::Value;

/// `validate_parameters(params, shape)`: the full
/// `ParamValidationError` message, or `None` when the parameters are
/// valid.
pub(crate) fn validate(params: &Value, shape: &Shape<'_>) -> Option<String> {
    report(params, shape).map(|r| format!("Parameter validation failed:\n{r}"))
}

/// `ValidationErrors.generate_report()`: the lines after the
/// `ParamValidationError` heading, or `None` when the parameters are
/// valid.
pub(crate) fn report(params: &Value, shape: &Shape<'_>) -> Option<String> {
    let mut errors = Vec::new();
    validate_value(params, shape, &mut errors, "");
    if errors.is_empty() {
        None
    } else {
        Some(errors.join("\n"))
    }
}

/// `ValidationErrors._get_name`.
fn display_name(name: &str) -> &str {
    if name.is_empty() {
        "input"
    } else if let Some(n) = name.strip_prefix('.') {
        n
    } else {
        name
    }
}

fn class(t: &str) -> String {
    format!("<class '{t}'>")
}

fn type_of(v: &Value) -> String {
    class(py_type_name(v))
}

fn invalid_type(errors: &mut Vec<String>, name: &str, param: &Value, valid: &[String]) {
    errors.push(format!(
        "Invalid type for parameter {}, value: {}, type: {}, valid types: {}",
        display_name(name),
        py_str(param),
        type_of(param),
        valid.join(", ")
    ));
}

/// `str()` of a model `min` value.
fn json_number_str(v: &Json) -> String {
    match v {
        Json::Number(n) => {
            let s = n.to_string();
            if s.contains(['.', 'e', 'E']) {
                float_repr(s.parse::<f64>().unwrap_or(f64::NAN))
            } else {
                s
            }
        }
        other => other.to_string(),
    }
}

/// Python `value < min_allowed` for numbers.
fn less_than(value: &Value, min: &Json) -> bool {
    let Some(min) = min.as_f64() else {
        return false;
    };
    match value {
        Value::Int(i) => {
            if let Some(mi) = min_as_bigint(min) {
                i < &mi
            } else {
                num_traits::ToPrimitive::to_f64(i).is_some_and(|f| f < min)
            }
        }
        Value::Float(f) => *f < min,
        Value::Bool(b) => (i64::from(*b) as f64) < min,
        _ => false,
    }
}

fn min_as_bigint(min: f64) -> Option<num_bigint::BigInt> {
    if min.fract() == 0.0 {
        num_traits::FromPrimitive::from_f64(min)
    } else {
        None
    }
}

/// `range_check`.
fn range_check(
    name: &str,
    value: &Value,
    shape: &Shape<'_>,
    length: bool,
    errors: &mut Vec<String>,
) {
    let (failed, min_str) = if let Some(min) = shape.shape_get("min") {
        (less_than(value, min), json_number_str(min))
    } else if shape
        .member_get("hostLabel")
        .and_then(Json::as_bool)
        .unwrap_or(false)
    {
        (less_than(value, &Json::from(1)), "1".to_string())
    } else {
        (false, String::new())
    };
    if failed {
        let what = if length { "length" } else { "value" };
        let kind = if length { "length" } else { "value" };
        errors.push(format!(
            "Invalid {what} for parameter {}, value: {}, valid min {kind}: {min_str}",
            display_name(name),
            py_str(value),
        ));
    }
}

fn is_json_value_header(shape: &Shape<'_>) -> bool {
    shape.get_bool("jsonvalue")
        && shape.get_str("location") == Some("header")
        && shape.type_name() == "string"
}

fn validate_value(params: &Value, shape: &Shape<'_>, errors: &mut Vec<String>, name: &str) {
    if is_json_value_header(shape) {
        if let Some(t) = unserializable(params) {
            errors.push(format!(
                "Invalid parameter {} must be json serializable: Object of type {t} is not JSON serializable",
                display_name(name)
            ));
        }
        return;
    }
    if shape.type_name() == "structure" && shape.is_document() {
        validate_document(params, errors, name);
        return;
    }
    match shape.type_name() {
        "structure" => {
            let Value::Map(m) = params else {
                invalid_type(errors, name, params, &[class("dict")]);
                return;
            };
            let members = shape.members();
            if shape.is_union() {
                let keys: Vec<&str> = members.iter().map(|(n, _)| *n).collect();
                if m.is_empty() {
                    errors.push(format!(
                        "Must set one of the following keys for tagged unionstructure {}: {}.",
                        display_name(name),
                        keys.join(". ")
                    ));
                } else if m.len() > 1 {
                    errors.push(format!(
                        "Invalid number of parameters set for tagged union structure {}. Can only set one of the following keys: {}.",
                        display_name(name),
                        keys.join(". ")
                    ));
                }
            }
            for required in shape.required() {
                if !m.contains_key(required) {
                    errors.push(format!(
                        "Missing required parameter in {}: \"{required}\"",
                        display_name(name)
                    ));
                }
            }
            let valid: Vec<&str> = members.iter().map(|(n, _)| *n).collect();
            let mut known = Vec::new();
            for key in m.keys() {
                if valid.contains(&key.as_str()) {
                    known.push(key);
                } else {
                    errors.push(format!(
                        "Unknown parameter in {}: \"{key}\", must be one of: {}",
                        display_name(name),
                        valid.join(", ")
                    ));
                }
            }
            for key in known {
                let member = shape.member_shape(key).expect("member");
                validate_value(&m[key], &member, errors, &format!("{name}.{key}"));
            }
        }
        "string" => match params {
            Value::Str(s) => {
                range_check(
                    name,
                    &Value::int(s.chars().count() as i64),
                    shape,
                    true,
                    errors,
                );
            }
            _ => invalid_type(errors, name, params, &[class("str")]),
        },
        "list" => match params {
            Value::List(items) => {
                range_check(name, &Value::int(items.len() as i64), shape, true, errors);
                if let Some(member) = shape.list_member() {
                    for (i, item) in items.iter().enumerate() {
                        validate_value(item, &member, errors, &format!("{name}[{i}]"));
                    }
                }
            }
            _ => invalid_type(errors, name, params, &[class("list"), class("tuple")]),
        },
        "map" => match params {
            Value::Map(m) => {
                let key_shape = shape.map_key();
                let value_shape = shape.map_value();
                for (k, v) in m {
                    if let Some(ks) = &key_shape {
                        validate_value(
                            &Value::Str(k.clone()),
                            ks,
                            errors,
                            &format!("{name} (key: {k})"),
                        );
                    }
                    if let Some(vs) = &value_shape {
                        validate_value(v, vs, errors, &format!("{name}.{k}"));
                    }
                }
            }
            _ => invalid_type(errors, name, params, &[class("dict")]),
        },
        "integer" | "long" => match params {
            Value::Int(_) | Value::Bool(_) => range_check(name, params, shape, false, errors),
            _ => invalid_type(errors, name, params, &[class("int")]),
        },
        "double" | "float" => match params {
            Value::Int(_) | Value::Bool(_) | Value::Float(_) => {
                range_check(name, params, shape, false, errors)
            }
            _ => invalid_type(
                errors,
                name,
                params,
                &[class("float"), class("decimal.Decimal"), class("int")],
            ),
        },
        "boolean" => {
            if !matches!(params, Value::Bool(_)) {
                invalid_type(errors, name, params, &[class("bool")]);
            }
        }
        "blob" => {
            if !matches!(params, Value::Bytes(_) | Value::Str(_)) {
                invalid_type(
                    errors,
                    name,
                    params,
                    &[
                        class("bytes"),
                        class("bytearray"),
                        "file-like object".to_string(),
                    ],
                );
            }
        }
        "timestamp" if !is_timestamp(params) => {
            invalid_type(
                errors,
                name,
                params,
                &[class("datetime.datetime"), "timestamp-string".to_string()],
            );
        }
        _ => {}
    }
}

/// `_type_check_datetime` (`parse_to_aware_datetime` succeeds).
fn is_timestamp(v: &Value) -> bool {
    match v {
        Value::Bool(_) => true,
        Value::Int(_) | Value::Float(_) | Value::Str(_) => {
            crate::protocol::timestamp::serialize(v, "iso8601").is_ok()
        }
        _ => false,
    }
}

/// The type `json.dumps` cannot serialize, if any.
fn unserializable(v: &Value) -> Option<&'static str> {
    match v {
        Value::Bytes(_) => Some("bytes"),
        Value::List(l) => l.iter().find_map(unserializable),
        Value::Map(m) => m.values().find_map(unserializable),
        _ => None,
    }
}

/// `_validate_document`.
fn validate_document(params: &Value, errors: &mut Vec<String>, name: &str) {
    match params {
        Value::Null | Value::Str(_) | Value::Int(_) | Value::Bool(_) | Value::Float(_) => {}
        Value::Map(m) => {
            for (k, v) in m {
                validate_document(v, errors, k);
            }
        }
        Value::List(l) => {
            for (i, v) in l.iter().enumerate() {
                validate_document(v, errors, &format!("{name}[{i}]"));
            }
        }
        Value::Bytes(_) => {
            let valid: Vec<String> = ["str", "int", "bool", "float", "list", "dict"]
                .iter()
                .map(|t| class(t))
                .collect();
            errors.push(format!(
                "Invalid type for document parameter {}, value: {}, type: {}, valid types: {}",
                display_name(name),
                py_str(params),
                type_of(params),
                valid.join(", ")
            ));
        }
    }
}
