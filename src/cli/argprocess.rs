//! Turning command line strings into parameter values
//! (`awscli/argprocess.py`: `unpack_cli_arg`, `ParamShorthandParser`;
//! `awscli/paramfile.py` `URIArgumentHandler`).

use super::Fail;
use super::paramfile;
use super::pyjson;
use super::pynum::py_strip;
use super::shorthand::{ShorthandError, ShorthandParser, back_compat_visit, float_of, int_of};
use crate::model::Shape;
use crate::pyfmt::py_str;
use crate::value::{Map, Value};

/// `ParamError(cli_name, message)`.
pub(crate) fn param_error(cli_name: &str, message: &str) -> Fail {
    Fail::ParamValidation(format!("Error parsing parameter '{cli_name}': {message}"))
}

/// The `load-cli-arg` handler (`URIArgumentHandler`): the file contents
/// when the value is a `file://` / `fileb://` reference.
pub(crate) fn load_cli_arg(cli_name: &str, value: &Value) -> Result<Option<Value>, Fail> {
    let value = match value {
        Value::List(l) if l.len() == 1 => &l[0],
        v => v,
    };
    let Value::Str(s) = value else {
        return Ok(None);
    };
    paramfile::get_paramfile(s).map_err(|e| param_error(cli_name, &e.0))
}

/// `utils.is_document_type_container`.
pub(crate) fn is_document_type_container(shape: &Shape<'_>) -> bool {
    fn walk<'a>(s: &Shape<'a>, stack: &mut Vec<String>, visited: &mut Vec<Shape<'a>>) {
        if stack.iter().any(|n| n == s.name) {
            return;
        }
        stack.push(s.name.to_string());
        visited.push(*s);
        match s.type_name() {
            "structure" => {
                for (_, m) in s.members() {
                    walk(&m, stack, visited);
                }
            }
            "list" => {
                if let Some(m) = s.list_member() {
                    walk(&m, stack, visited);
                }
            }
            "map" => {
                if let Some(v) = s.map_value() {
                    walk(&v, stack, visited);
                }
            }
            _ => {}
        }
        stack.pop();
    }
    let mut visited = Vec::new();
    walk(shape, &mut Vec::new(), &mut visited);
    let Some(end) = visited.pop() else {
        return false;
    };
    if !(end.type_name() == "structure" && end.is_document()) {
        return false;
    }
    visited
        .iter()
        .all(|s| matches!(s.type_name(), "list" | "map"))
}

fn is_document(shape: &Shape<'_>) -> bool {
    shape.type_name() == "structure" && shape.is_document()
}

fn is_json_value_header(shape: &Shape<'_>) -> bool {
    shape.get_bool("jsonvalue")
        && shape.get_str("location") == Some("header")
        && shape.type_name() == "string"
}

/// `_supports_shorthand_syntax`.
fn supports_shorthand(shape: &Shape<'_>) -> bool {
    if is_document_type_container(shape) {
        return false;
    }
    match shape.type_name() {
        "structure" | "map" => true,
        "list" => shape
            .list_member()
            .is_some_and(|m| matches!(m.type_name(), "structure" | "list" | "map")),
        _ => false,
    }
}

/// `ParamShorthand._uses_old_list_case`.
fn uses_old_list_case(service: &str, operation: &str, argument: &str) -> bool {
    let cases: &[(&str, &str, &str)] = &[
        ("firehose", "put-record-batch", "records"),
        (
            "workspaces",
            "reboot-workspaces",
            "reboot-workspace-requests",
        ),
        (
            "workspaces",
            "rebuild-workspaces",
            "rebuild-workspace-requests",
        ),
        (
            "workspaces",
            "terminate-workspaces",
            "terminate-workspace-requests",
        ),
        ("elb", "remove-tags", "tags"),
        ("elb", "describe-instance-health", "instances"),
        (
            "elb",
            "deregister-instances-from-load-balancer",
            "instances",
        ),
        ("elb", "register-instances-with-load-balancer", "instances"),
    ];
    cases
        .iter()
        .any(|(s, o, a)| *s == service && *o == operation && *a == argument)
}

/// The argument a value is being unpacked for.
pub(crate) struct ArgContext<'a, 'm> {
    pub name: &'a str,
    pub cli_name: &'a str,
    pub shape: &'a Shape<'m>,
    /// The botocore service name and the `xform_name` of the operation
    /// (the `process-cli-arg.<service>.<operation>` event).
    pub service: &'a str,
    pub operation: &'a str,
}

/// `CLIArgument._unpack_argument`: the `process-cli-arg` handlers
/// (`ParamShorthandParser`), then `unpack_cli_arg`.
pub(crate) fn unpack_argument(arg: &ArgContext<'_, '_>, value: &Value) -> Result<Value, Fail> {
    if let Some(v) = shorthand(arg, value)? {
        return Ok(v);
    }
    unpack_cli_arg(arg.shape, value, arg.cli_name)
}

/// `ParamShorthandParser.__call__`.
fn shorthand(arg: &ArgContext<'_, '_>, value: &Value) -> Result<Option<Value>, Fail> {
    let check = match value {
        Value::List(l) if !l.is_empty() => &l[0],
        v => v,
    };
    if let Value::Str(s) = check {
        let t = py_strip(s);
        if t.starts_with('[') || t.starts_with('{') {
            return Ok(None);
        }
    }
    let model = arg.shape;
    if !supports_shorthand(model) {
        return Ok(None);
    }
    let map_err = |e: ShorthandError| match e {
        ShorthandError::Parse(m) => param_error(arg.cli_name, &m),
        ShorthandError::DocumentType => param_error(arg.cli_name, "document"),
        ShorthandError::Paramfile(p) => Fail::General(p.0),
        ShorthandError::Python(m) => Fail::General(m),
    };
    if model.type_name() == "list"
        && let Some(member) = model.list_member()
        && member.type_name() == "structure"
        && member.members().len() == 1
        && uses_old_list_case(arg.service, arg.operation, arg.name)
    {
        let key = member.members()[0].0.to_string();
        let items = match value {
            Value::List(l) => l.clone(),
            Value::Str(s) => s.chars().map(|c| Value::Str(c.to_string())).collect(),
            _ => Vec::new(),
        };
        return Ok(Some(Value::List(
            items
                .into_iter()
                .map(|v| {
                    let mut m = Map::new();
                    m.insert(key.clone(), v);
                    Value::Map(m)
                })
                .collect(),
        )));
    }
    if model.type_name() == "structure" {
        let members = model.members();
        let single_value =
            members.len() == 1 && members[0].0 == "Value" && members[0].1.type_name() == "string";
        if single_value && matches!(value, Value::Bytes(_)) {
            return Err(Fail::Python(
                "a bytes-like object is required, not 'str'".to_string(),
            ));
        }
        if single_value
            && let Value::Str(s) = value
            && !s.contains('=')
        {
            let mut m = Map::new();
            m.insert("Value".to_string(), value.clone());
            return Ok(Some(Value::Map(m)));
        }
    }
    let mut parsed = match value {
        Value::List(items) => {
            let mut out = Vec::new();
            for v in items {
                let s = match v {
                    Value::Str(s) => s.as_str(),
                    _ => return Err(Fail::General("expected string".to_string())),
                };
                out.push(ShorthandParser::parse(s).map_err(map_err)?);
            }
            Value::List(out)
        }
        Value::Str(s) => ShorthandParser::parse(s).map_err(map_err)?,
        Value::Bytes(b) if !b.is_empty() => {
            return Err(Fail::Python(
                "'in <string>' requires string as left operand, not int".to_string(),
            ));
        }
        _ => return Ok(None),
    };
    back_compat_visit(&mut parsed, model).map_err(map_err)?;
    Ok(Some(parsed))
}

/// `_unpack_json_cli_arg`.
fn unpack_json(value: &Value, cli_name: &str) -> Result<Value, Fail> {
    let text = match value {
        Value::Str(s) => s.clone(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => {
            return Err(Fail::General(format!(
                "the JSON object must be str, bytes or bytearray, not {}",
                super::shorthand::py_type_name(other)
            )));
        }
    };
    pyjson::loads(&text).map_err(|e| {
        param_error(
            cli_name,
            &format!("Invalid JSON: {e}\nJSON received: {}", py_str(value)),
        )
    })
}

/// Whether a failure is a Python `ValueError` / `TypeError` (what the
/// list branch of `_unpack_complex_cli_arg` catches).
fn is_value_or_type_error(f: &Fail) -> bool {
    matches!(f, Fail::Python(_))
}

/// `unpack_cli_arg` / `_unpack_cli_arg`.
pub(crate) fn unpack_cli_arg(
    shape: &Shape<'_>,
    value: &Value,
    cli_name: &str,
) -> Result<Value, Fail> {
    if is_json_value_header(shape) || is_document(shape) {
        return unpack_json(value, cli_name);
    }
    let t = shape.type_name();
    if super::arguments::is_scalar_type(t) {
        return unpack_scalar(shape, value, cli_name);
    }
    match t {
        "structure" | "map" => {
            let first = match value {
                Value::Str(s) => match py_strip_start(s).chars().next() {
                    Some(c) => Some(c),
                    None => return Err(Fail::General("string index out of range".to_string())),
                },
                _ => None,
            };
            if first == Some('{') {
                return unpack_json(value, cli_name);
            }
            Err(param_error(
                cli_name,
                &format!("Invalid JSON:\n{}", py_str(value)),
            ))
        }
        "list" => {
            match value {
                Value::Str(s) => match py_strip_start(s).chars().next() {
                    Some('[') => return unpack_json(value, cli_name),
                    Some(_) => {}
                    None => return Err(Fail::General("string index out of range".to_string())),
                },
                Value::List(l) if l.len() == 1 => {
                    if let Value::Str(s) = &l[0]
                        && py_strip(s).starts_with('[')
                    {
                        return unpack_json(&l[0], cli_name);
                    }
                }
                _ => {}
            }
            let member = shape.list_member().expect("list member");
            let items: Vec<Value> = match value {
                Value::List(l) => l.clone(),
                Value::Str(s) => s.chars().map(|c| Value::Str(c.to_string())).collect(),
                Value::Bytes(b) => b.iter().map(|&x| Value::int(i64::from(x))).collect(),
                other => {
                    return Err(Fail::Python(format!(
                        "'{}' object is not iterable",
                        super::shorthand::py_type_name(other)
                    )));
                }
            };
            let mut out = Vec::new();
            for v in &items {
                match unpack_cli_arg(&member, v, cli_name) {
                    Ok(x) => out.push(x),
                    Err(e) if is_value_or_type_error(&e) => {
                        let first = items.first().map(py_str).unwrap_or_default();
                        return Err(param_error(cli_name, &first));
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(Value::List(out))
        }
        _ => Ok(Value::Str(py_str(value))),
    }
}

fn py_strip_start(s: &str) -> &str {
    s.trim_start_matches(super::pynum::is_py_space)
}

/// `unpack_scalar_cli_arg`.
fn unpack_scalar(shape: &Shape<'_>, value: &Value, cli_name: &str) -> Result<Value, Fail> {
    match shape.type_name() {
        "integer" | "long" => int_of(value).map_err(Fail::Python),
        "float" | "double" => float_of(value).map_err(Fail::Python),
        "blob" if shape.is_streaming() => {
            let s = match value {
                Value::Str(s) => s.clone(),
                Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                other => {
                    return Err(Fail::Python(format!(
                        "expected str, bytes or os.PathLike object, not {}",
                        super::shorthand::py_type_name(other)
                    )));
                }
            };
            let path = paramfile::expanduser(&paramfile::expandvars(&s));
            if !std::path::Path::new(&path).is_file() {
                return Err(param_error(
                    cli_name,
                    "Blob values must be a path to a file.",
                ));
            }
            std::fs::read(&path)
                .map(Value::Bytes)
                .map_err(|e| Fail::General(paramfile::os_error(&e, &path)))
        }
        "boolean" => match value {
            Value::Str(s) if s.to_lowercase() == "false" => Ok(Value::Bool(false)),
            v => Ok(Value::Bool(v.is_truthy())),
        },
        _ => Ok(value.clone()),
    }
}
