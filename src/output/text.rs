//! The text output format (`awscli/text.py`).

use std::collections::BTreeSet;

use crate::pyfmt::py_str;
use crate::value::{Map, Value};

/// `text.format_text`. Output written before a failure stays in `out`
/// (Python writes to the stream as it goes); the error carries Python's
/// exception message.
pub fn format_text(data: &Value, out: &mut String) -> Result<(), String> {
    format_value(data, out, None, None)
}

/// Python's `type(v).__name__`.
pub(crate) fn type_name(v: &Value) -> &'static str {
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

/// `text._format_text`.
fn format_value(
    item: &Value,
    out: &mut String,
    identifier: Option<&str>,
    scalar_keys: Option<&[String]>,
) -> Result<(), String> {
    match item {
        Value::Map(m) => format_dict(scalar_keys, m, identifier, out),
        Value::List(l) => format_list(l, identifier, out),
        other => {
            out.push_str(&py_str(other));
            out.push('\n');
            Ok(())
        }
    }
}

/// `text._format_list`.
fn format_list(item: &[Value], identifier: Option<&str>, out: &mut String) -> Result<(), String> {
    if item.is_empty() {
        return Ok(());
    }
    if item.iter().any(|el| matches!(el, Value::Map(_))) {
        let all_keys = all_scalar_keys(item)?;
        for element in item {
            format_value(element, out, identifier, Some(&all_keys))?;
        }
    } else if item.iter().any(|el| matches!(el, Value::List(_))) {
        let (scalars, non_scalars): (Vec<&Value>, Vec<&Value>) =
            item.iter().partition(|el| el.is_scalar());
        if !scalars.is_empty() {
            format_scalar_list(&scalars, identifier, out);
        }
        for non_scalar in non_scalars {
            format_value(non_scalar, out, identifier, None)?;
        }
    } else {
        let all: Vec<&Value> = item.iter().collect();
        format_scalar_list(&all, identifier, out);
    }
    Ok(())
}

/// `text._format_scalar_list`.
fn format_scalar_list(elements: &[&Value], identifier: Option<&str>, out: &mut String) {
    match identifier {
        Some(id) => {
            let upper = id.to_uppercase();
            for item in elements {
                out.push_str(&upper);
                out.push('\t');
                out.push_str(&py_str(item));
                out.push('\n');
            }
        }
        None => {
            let parts: Vec<String> = elements.iter().map(|v| py_str(v)).collect();
            out.push_str(&parts.join("\t"));
            out.push('\n');
        }
    }
}

/// `text._format_dict`.
fn format_dict(
    scalar_keys: Option<&[String]>,
    item: &Map,
    identifier: Option<&str>,
    out: &mut String,
) -> Result<(), String> {
    let (mut scalars, non_scalars) = partition_dict(item, scalar_keys);
    if !scalars.is_empty() {
        if let Some(id) = identifier {
            scalars.insert(0, id.to_uppercase());
        }
        out.push_str(&scalars.join("\t"));
        out.push('\n');
    }
    for (new_identifier, non_scalar) in non_scalars {
        format_value(non_scalar, out, Some(new_identifier), None)?;
    }
    Ok(())
}

/// `text._all_scalar_keys`.
fn all_scalar_keys(list_of_dicts: &[Value]) -> Result<Vec<String>, String> {
    let mut keys = BTreeSet::new();
    for item in list_of_dicts {
        let Value::Map(m) = item else {
            return Err(format!(
                "'{}' object has no attribute 'items'",
                type_name(item)
            ));
        };
        for (k, v) in m {
            if v.is_scalar() {
                keys.insert(k.clone());
            }
        }
    }
    Ok(keys.into_iter().collect())
}

/// `text._partition_dict`.
fn partition_dict<'a>(
    item: &'a Map,
    scalar_keys: Option<&[String]>,
) -> (Vec<String>, Vec<(&'a str, &'a Value)>) {
    let mut scalar = Vec::new();
    let mut non_scalar = Vec::new();
    match scalar_keys {
        None => {
            let mut entries: Vec<(&String, &Value)> = item.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (k, v) in entries {
                if v.is_scalar() {
                    scalar.push(py_str(v));
                } else {
                    non_scalar.push((k.as_str(), v));
                }
            }
        }
        Some(keys) => {
            for key in keys {
                scalar.push(item.get(key).map(py_str).unwrap_or_default());
            }
            let mut remaining: Vec<&String> = item.keys().filter(|k| !keys.contains(k)).collect();
            remaining.sort();
            for k in remaining {
                non_scalar.push((k.as_str(), &item[k.as_str()]));
            }
        }
    }
    (scalar, non_scalar)
}
