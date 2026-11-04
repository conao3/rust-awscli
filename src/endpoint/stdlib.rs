//! Rule set values and the standard library
//! (`botocore/endpoint_provider.py` `RuleSetStandardLibary`).

use std::sync::OnceLock;

use indexmap::IndexMap;
use regex::Regex;

use crate::bjson::{Node, Obj};

use super::EndpointError;
use super::pyurl;

/// A Python value as it appears in the rule set scope: parameters
/// (`str`, `bool`, `tuple` for string arrays), function results (`dict`
/// from `aws.partition` / `aws.parseArn` / `parseURL`, `list` for
/// `resourceId`), and literals.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Val>),
    Tuple(Vec<Val>),
    Dict(IndexMap<String, Val>),
}

impl Val {
    pub fn from_node(v: &Node) -> Val {
        if let Some(a) = v.as_array() {
            return Val::List(a.iter().map(Val::from_node).collect());
        }
        if let Some(o) = v.as_object() {
            return Val::Dict(
                o.iter()
                    .map(|(k, v)| (k.to_string(), Val::from_node(v)))
                    .collect(),
            );
        }
        if let Some(s) = v.as_str() {
            return Val::Str(s.to_string());
        }
        if let Some(b) = v.as_bool() {
            return Val::Bool(b);
        }
        if v.is_number() {
            return match v.as_i64() {
                Some(i) => Val::Int(i),
                None => Val::Float(v.as_f64().unwrap_or(f64::NAN)),
            };
        }
        Val::None
    }

    pub fn from_json(v: &serde_json::Value) -> Val {
        match v {
            serde_json::Value::Null => Val::None,
            serde_json::Value::Bool(b) => Val::Bool(*b),
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => Val::Int(i),
                None => Val::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
            serde_json::Value::String(s) => Val::Str(s.clone()),
            serde_json::Value::Array(a) => Val::List(a.iter().map(Val::from_json).collect()),
            serde_json::Value::Object(o) => Val::Dict(
                o.iter()
                    .map(|(k, v)| (k.clone(), Val::from_json(v)))
                    .collect(),
            ),
        }
    }

    /// Python `type(value)` as printed by `f"{type(value)}"`.
    pub fn type_repr(&self) -> &'static str {
        match self {
            Val::None => "<class 'NoneType'>",
            Val::Bool(_) => "<class 'bool'>",
            Val::Int(_) => "<class 'int'>",
            Val::Float(_) => "<class 'float'>",
            Val::Str(_) => "<class 'str'>",
            Val::List(_) => "<class 'list'>",
            Val::Tuple(_) => "<class 'tuple'>",
            Val::Dict(_) => "<class 'dict'>",
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            Val::None => "NoneType",
            Val::Bool(_) => "bool",
            Val::Int(_) => "int",
            Val::Float(_) => "float",
            Val::Str(_) => "str",
            Val::List(_) => "list",
            Val::Tuple(_) => "tuple",
            Val::Dict(_) => "dict",
        }
    }

    /// Python truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            Val::None => false,
            Val::Bool(b) => *b,
            Val::Int(i) => *i != 0,
            Val::Float(f) => *f != 0.0,
            Val::Str(s) => !s.is_empty(),
            Val::List(l) | Val::Tuple(l) => !l.is_empty(),
            Val::Dict(d) => !d.is_empty(),
        }
    }

    /// Python `str(value)`.
    pub fn py_str(&self) -> String {
        match self {
            Val::Str(s) => s.clone(),
            other => other.py_repr(),
        }
    }

    /// Python `repr(value)`.
    pub fn py_repr(&self) -> String {
        match self {
            Val::None => "None".into(),
            Val::Bool(true) => "True".into(),
            Val::Bool(false) => "False".into(),
            Val::Int(i) => i.to_string(),
            Val::Float(f) => crate::pyfmt::float_repr(*f),
            Val::Str(s) => crate::pyfmt::str_repr(s),
            Val::List(l) => format!(
                "[{}]",
                l.iter().map(Val::py_repr).collect::<Vec<_>>().join(", ")
            ),
            Val::Tuple(l) if l.len() == 1 => format!("({},)", l[0].py_repr()),
            Val::Tuple(l) => format!(
                "({})",
                l.iter().map(Val::py_repr).collect::<Vec<_>>().join(", ")
            ),
            Val::Dict(d) => format!(
                "{{{}}}",
                d.iter()
                    .map(|(k, v)| format!("{}: {}", crate::pyfmt::str_repr(k), v.py_repr()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

fn internal(msg: impl Into<String>) -> EndpointError {
    EndpointError::Internal(msg.into())
}

fn resolution(msg: impl Into<String>) -> EndpointError {
    EndpointError::Resolution(msg.into())
}

pub type Scope = IndexMap<String, Val>;

/// `TEMPLATE_STRING_RE.search(value)`.
pub fn is_template(s: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\{[a-zA-Z#]+\}").unwrap())
        .is_match(s)
}

/// `string.Formatter().parse(value)`: the literal text and the field
/// names, in order.
fn formatter_parse(s: &str) -> Result<Vec<(String, Option<String>)>, EndpointError> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut literal = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' {
            if chars.get(i + 1) == Some(&'{') {
                literal.push('{');
                out.push((std::mem::take(&mut literal), None));
                i += 2;
                continue;
            }
            let mut depth = 1;
            let mut j = i + 1;
            while j < chars.len() {
                match chars[j] {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            if j >= chars.len() {
                return Err(internal("expected '}' before end of string"));
            }
            let field: String = chars[i + 1..j].iter().collect();
            let name_end = field.find(['!', ':']).unwrap_or(field.len());
            let rest = &field[name_end..];
            if let Some(conv) = rest.strip_prefix('!') {
                let mut it = conv.chars();
                if it.next().is_none() {
                    return Err(internal(
                        "end of string while looking for conversion specifier",
                    ));
                }
                if let Some(next) = it.next()
                    && next != ':'
                {
                    return Err(internal("expected ':' after conversion specifier"));
                }
            }
            out.push((
                std::mem::take(&mut literal),
                Some(field[..name_end].to_string()),
            ));
            i = j + 1;
        } else if c == '}' {
            if chars.get(i + 1) == Some(&'}') {
                literal.push('}');
                out.push((std::mem::take(&mut literal), None));
                i += 2;
                continue;
            }
            return Err(internal("Single '}' encountered in format string"));
        } else {
            literal.push(c);
            i += 1;
        }
    }
    if !literal.is_empty() {
        out.push((literal, None));
    }
    Ok(out)
}

/// `RuleSetStandardLibary.resolve_template_string`.
pub fn resolve_template_string(value: &str, scope: &Scope) -> Result<String, EndpointError> {
    let mut result = String::new();
    for (literal, reference) in formatter_parse(value)? {
        result.push_str(&literal);
        if let Some(reference) = reference {
            let mut parts = reference.split('#');
            let first = parts.next().unwrap_or_default();
            let mut current = scope
                .get(first)
                .ok_or_else(|| internal(crate::pyfmt::str_repr(first)))?;
            for part in parts {
                current = subscript(current, part)?;
            }
            result.push_str(&current.py_str());
        }
    }
    Ok(result)
}

/// Python `value[key]` with a string key.
fn subscript<'a>(value: &'a Val, key: &str) -> Result<&'a Val, EndpointError> {
    match value {
        Val::Dict(d) => d
            .get(key)
            .ok_or_else(|| internal(crate::pyfmt::str_repr(key))),
        Val::List(_) | Val::Tuple(_) | Val::Str(_) => Err(internal(format!(
            "{} indices must be integers or slices, not str",
            value.type_name()
        ))),
        other => Err(internal(format!(
            "'{}' object is not subscriptable",
            other.type_name()
        ))),
    }
}

/// `RuleSetStandardLibary.resolve_value`.
pub fn resolve_value(value: &Node, scope: &mut Scope) -> Result<Val, EndpointError> {
    if let Some(obj) = value.as_object() {
        if obj.contains_key("fn") {
            return call_function(obj, scope);
        }
        if let Some(r) = obj.get("ref") {
            let name = r.as_str().unwrap_or_default();
            return Ok(scope.get(name).cloned().unwrap_or(Val::None));
        }
    }
    if let Some(s) = value.as_str()
        && is_template(s)
    {
        return Ok(Val::Str(resolve_template_string(s, scope)?));
    }
    Ok(Val::from_node(value))
}

/// `RuleSetStandardLibary.call_function`.
pub fn call_function(sig: &Obj, scope: &mut Scope) -> Result<Val, EndpointError> {
    let mut args = Vec::new();
    if let Some(argv) = sig.get("argv").and_then(|a| a.as_array()) {
        for arg in argv.iter() {
            args.push(resolve_value(arg, scope)?);
        }
    }
    let name = sig.get("fn").and_then(|f| f.as_str()).unwrap_or_default();
    let result = dispatch(name, args)?;
    if let Some(assign) = sig.get("assign").and_then(|a| a.as_str()) {
        if scope.contains_key(assign) {
            return Err(resolution(format!(
                "Assignment {assign} already exists in scoped variables and cannot be overwritten"
            )));
        }
        scope.insert(assign.to_string(), result.clone());
    }
    Ok(result)
}

fn arg_count_error(name: &str, expected: usize, got: usize) -> EndpointError {
    internal(format!(
        "RuleSetStandardLibary.{name}() takes {} positional arguments but {} were given",
        expected + 1,
        got + 1
    ))
}

fn dispatch(name: &str, args: Vec<Val>) -> Result<Val, EndpointError> {
    let (py_name, arity) = match name {
        "isSet" => ("is_set", 1),
        "not" => ("_not", 1),
        "getAttr" => ("get_attr", 2),
        "stringEquals" => ("string_equals", 2),
        "booleanEquals" => ("boolean_equals", 2),
        "substring" => ("substring", 4),
        "uriEncode" => ("uri_encode", 1),
        "parseURL" => ("parse_url", 1),
        "isValidHostLabel" => ("is_valid_host_label", 2),
        "aws.partition" => ("aws_partition", 1),
        "aws.parseArn" => ("aws_parse_arn", 1),
        "aws.isVirtualHostableS3Bucket" => ("aws_is_virtual_hostable_s3_bucket", 2),
        other => {
            return Err(internal(format!(
                "'RuleSetStandardLibary' object has no attribute '{}'",
                convert_func_name(other)
            )));
        }
    };
    if args.len() != arity {
        return Err(arg_count_error(py_name, arity, args.len()));
    }
    let mut it = args.into_iter();
    let mut next = || it.next().unwrap();
    match name {
        "isSet" => Ok(Val::Bool(next() != Val::None)),
        "not" => Ok(Val::Bool(!next().truthy())),
        "getAttr" => {
            let value = next();
            let path = next();
            get_attr(value, &path)
        }
        "stringEquals" => {
            let (a, b) = (next(), next());
            match (&a, &b) {
                (Val::Str(x), Val::Str(y)) => Ok(Val::Bool(x == y)),
                _ => Err(resolution(format!(
                    "Both values must be strings, not {} and {}.",
                    a.type_repr(),
                    b.type_repr()
                ))),
            }
        }
        "booleanEquals" => {
            let (a, b) = (next(), next());
            match (&a, &b) {
                (Val::Bool(x), Val::Bool(y)) => Ok(Val::Bool(x == y)),
                _ => Err(resolution(format!(
                    "Both arguments must be bools, not {} and {}.",
                    a.type_repr(),
                    b.type_repr()
                ))),
            }
        }
        "substring" => {
            let (value, start, stop, reverse) = (next(), next(), next(), next());
            substring(value, start, stop, reverse)
        }
        "uriEncode" => match next() {
            Val::None => Ok(Val::None),
            v => Ok(Val::Str(pyurl::quote(&v.py_str(), "-._~"))),
        },
        "parseURL" => parse_url(next()),
        "isValidHostLabel" => {
            let (value, allow) = (next(), next());
            Ok(Val::Bool(is_valid_host_label(&value, &allow)?))
        }
        "aws.partition" => aws_partition(next()),
        "aws.parseArn" => aws_parse_arn(next()),
        "aws.isVirtualHostableS3Bucket" => {
            let (value, allow) = (next(), next());
            Ok(Val::Bool(is_virtual_hostable_s3_bucket(&value, &allow)?))
        }
        _ => unreachable!(),
    }
}

/// `RuleSetStandardLibary.convert_func_name` (`xform_name` of the
/// function name, `not` → `_not`, `.` → `_`).
fn convert_func_name(name: &str) -> String {
    let n = super::xform_name(name, "_");
    let n = if n == "not" { "_not".to_string() } else { n };
    n.replace('.', "_")
}

fn ascii_index_part(part: &str) -> Option<(&str, &str)> {
    let b = part.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    (0..b.len()).find_map(|open| {
        if b[open] != b'[' {
            return None;
        }
        let digits = b[open + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if digits == 0 || b.get(open + 1 + digits) != Some(&b']') {
            return None;
        }
        let start = b[..open]
            .iter()
            .rposition(|&c| !word(c))
            .map_or(0, |i| i + 1);
        Some((&part[start..open], &part[open + 1..open + 1 + digits]))
    })
}

fn index_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\w*)\[(\d+)\]").unwrap())
}

/// `RuleSetStandardLibary.get_attr`.
fn get_attr(value: Val, path: &Val) -> Result<Val, EndpointError> {
    let path = match path {
        Val::Str(s) => s.clone(),
        other => {
            return Err(internal(format!(
                "'{}' object has no attribute 'split'",
                other.type_name()
            )));
        }
    };
    let mut value = value;
    for part in path.split('.') {
        let caps = if !part.contains('[') {
            None
        } else if part.is_ascii() {
            ascii_index_part(part)
        } else {
            index_re()
                .captures(part)
                .map(|c| (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str()))
        };
        if let Some((name, digits)) = caps {
            let index: usize = match digits.parse() {
                Ok(i) => i,
                Err(_) => return Ok(Val::None),
            };
            if !name.is_empty() {
                value = match value {
                    Val::Dict(mut d) => d.shift_remove(name).unwrap_or(Val::None),
                    other => {
                        return Err(internal(format!(
                            "'{}' object has no attribute 'get'",
                            other.type_name()
                        )));
                    }
                };
            }
            return match value {
                Val::None => Ok(Val::None),
                Val::List(l) | Val::Tuple(l) => Ok(l.into_iter().nth(index).unwrap_or(Val::None)),
                Val::Str(s) => Ok(s
                    .chars()
                    .nth(index)
                    .map(|c| Val::Str(c.to_string()))
                    .unwrap_or(Val::None)),
                Val::Dict(d) => {
                    if index >= d.len() {
                        Ok(Val::None)
                    } else {
                        Err(internal(index.to_string()))
                    }
                }
                other => Err(internal(format!(
                    "object of type '{}' has no len()",
                    other.type_name()
                ))),
            };
        }
        value = subscript(&value, part)?.clone();
    }
    Ok(value)
}

/// `RuleSetStandardLibary.substring`.
fn substring(value: Val, start: Val, stop: Val, reverse: Val) -> Result<Val, EndpointError> {
    let s = match value {
        Val::Str(s) => s,
        other => {
            return Err(resolution(format!(
                "Input must be a string, not {}.",
                other.type_repr()
            )));
        }
    };
    let as_int = |v: &Val| match v {
        Val::Int(i) => Ok(*i),
        Val::Bool(b) => Ok(*b as i64),
        other => Err(internal(format!(
            "'>=' not supported between instances of '{}' and 'int'",
            other.type_name()
        ))),
    };
    let (start, stop) = (as_int(&start)?, as_int(&stop)?);
    let len = s.chars().count() as i64;
    if start >= stop || len < stop || !s.is_ascii() {
        return Ok(Val::None);
    }
    let slice = |a: i64, b: i64| {
        let norm = |i: i64| {
            if i < 0 { (len + i).max(0) } else { i.min(len) }
        };
        let (a, b) = (norm(a) as usize, norm(b) as usize);
        if a >= b {
            String::new()
        } else {
            s[a..b].to_string()
        }
    };
    if reverse == Val::Bool(true) {
        return Ok(Val::Str(slice(len - stop, len - start)));
    }
    Ok(Val::Str(slice(start, stop)))
}

/// `RuleSetStandardLibary.parse_url`.
fn parse_url(value: Val) -> Result<Val, EndpointError> {
    let value = match value {
        Val::None => return Ok(Val::None),
        Val::Str(s) => s,
        other => {
            return Err(internal(format!(
                "'{}' object has no attribute 'decode'",
                other.type_name()
            )));
        }
    };
    let parsed = pyurl::urlparse(&value).map_err(|e| internal(e.0))?;
    if parsed.port().is_err() {
        return Ok(Val::None);
    }
    if !(parsed.scheme == "https" || parsed.scheme == "http") || !parsed.query.is_empty() {
        return Ok(Val::None);
    }
    let mut normalized = pyurl::quote(&pyurl::normalize_url_path(&parsed.path), "/");
    if !normalized.ends_with('/') {
        normalized.push('/');
    }
    let is_ip = pyurl::is_valid_ipv4_endpoint_url(&value).map_err(internal)?
        || pyurl::is_valid_ipv6_endpoint_url(&value).map_err(internal)?;
    let mut d = IndexMap::new();
    d.insert("scheme".into(), Val::Str(parsed.scheme.clone()));
    d.insert("authority".into(), Val::Str(parsed.netloc.clone()));
    d.insert("path".into(), Val::Str(parsed.path.clone()));
    d.insert("normalizedPath".into(), Val::Str(normalized));
    d.insert("isIp".into(), Val::Bool(is_ip));
    Ok(Val::Dict(d))
}

/// `VALID_HOST_LABEL_RE.match(value)`:
/// `^(?!-)[a-zA-Z\d-]{1,63}(?<!-)$` (`\d` is Unicode, `$` also matches
/// before a trailing newline).
fn host_label_re_match(value: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\A[a-zA-Z\d-]{1,63}\n?\z").unwrap());
    if !re.is_match(value) {
        return false;
    }
    let body = value.strip_suffix('\n').unwrap_or(value);
    !body.starts_with('-') && !body.ends_with('-')
}

fn str_arg<'a>(value: &'a Val, method: &str) -> Result<&'a str, EndpointError> {
    match value {
        Val::Str(s) => Ok(s),
        other => Err(internal(format!(
            "'{}' object has no attribute '{method}'",
            other.type_name()
        ))),
    }
}

/// `RuleSetStandardLibary.is_valid_host_label`.
fn is_valid_host_label(value: &Val, allow_subdomains: &Val) -> Result<bool, EndpointError> {
    if *value == Val::None {
        return Ok(false);
    }
    if *allow_subdomains == Val::Bool(false) && str_arg(value, "count")?.contains('.') {
        return Ok(false);
    }
    if *allow_subdomains == Val::Bool(true) {
        let s = str_arg(value, "split")?;
        return Ok(s.split('.').all(host_label_re_match));
    }
    match value {
        Val::Str(s) => Ok(host_label_re_match(s)),
        other => Err(internal(format!(
            "expected string or bytes-like object, got '{}'",
            other.type_name()
        ))),
    }
}

/// `RuleSetStandardLibary.aws_is_virtual_hostable_s3_bucket`.
fn is_virtual_hostable_s3_bucket(
    value: &Val,
    allow_subdomains: &Val,
) -> Result<bool, EndpointError> {
    let s = match value {
        Val::None => return Ok(false),
        Val::Str(s) => s.as_str(),
        other => {
            return Err(internal(format!(
                "object of type '{}' has no len()",
                other.type_name()
            )));
        }
    };
    if s.chars().count() < 3 || pyurl::py_lower(s) != s || pyurl::is_ipv4(s) {
        return Ok(false);
    }
    if *allow_subdomains == Val::Bool(true) {
        for label in s.split('.') {
            if !is_virtual_hostable_s3_bucket(&Val::Str(label.to_string()), &Val::Bool(false))? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    is_valid_host_label(value, &Val::Bool(false))
}

struct Partition {
    region_regex: &'static str,
    regex: OnceLock<Regex>,
    regions: Vec<String>,
    output: IndexMap<String, Val>,
}

/// Translate a Python `re.match` pattern: anchor at the start, and let a
/// trailing `$` also match before a final newline.
fn python_match_regex(pattern: &str) -> Regex {
    let translated = match pattern.strip_suffix('$') {
        Some(body) if !body.ends_with('\\') => format!(r"\A(?:{body}\n?\z)"),
        _ => format!(r"\A(?:{pattern})"),
    };
    Regex::new(&translated).expect("partition regionRegex")
}

fn partitions() -> &'static Vec<Partition> {
    static P: OnceLock<Vec<Partition>> = OnceLock::new();
    P.get_or_init(|| {
        let data = crate::model::partitions();
        data["partitions"]
            .as_array()
            .expect("partitions")
            .iter()
            .map(|p| {
                let mut output = match Val::from_node(&p["outputs"]) {
                    Val::Dict(d) => d,
                    _ => IndexMap::new(),
                };
                output.insert(
                    "name".into(),
                    Val::Str(p["id"].as_str().unwrap_or_default().to_string()),
                );
                Partition {
                    region_regex: p["regionRegex"].as_str().unwrap_or_default(),
                    regex: OnceLock::new(),
                    regions: p["regions"]
                        .as_object()
                        .map(|r| r.keys().map(str::to_string).collect())
                        .unwrap_or_default(),
                    output,
                }
            })
            .collect()
    })
}

/// `RuleSetStandardLibary.aws_partition`.
fn aws_partition(value: Val) -> Result<Val, EndpointError> {
    let parts = partitions();
    match &value {
        Val::None => {}
        Val::Str(region) => {
            for p in parts {
                if p.regions.iter().any(|r| r == region)
                    || p.regex
                        .get_or_init(|| python_match_regex(p.region_regex))
                        .is_match(region)
                {
                    return Ok(Val::Dict(p.output.clone()));
                }
            }
        }
        other => {
            return Err(internal(format!(
                "expected string or bytes-like object, got '{}'",
                other.type_name()
            )));
        }
    }
    Ok(Val::Dict(parts[0].output.clone()))
}

/// `RuleSetStandardLibary.aws_parse_arn` (with `utils.ArnParser`).
fn aws_parse_arn(value: Val) -> Result<Val, EndpointError> {
    let s = match &value {
        Val::None => return Ok(Val::None),
        Val::Str(s) => s.as_str(),
        other => {
            return Err(internal(format!(
                "'{}' object has no attribute 'startswith'",
                other.type_name()
            )));
        }
    };
    if !s.starts_with("arn:") {
        return Ok(Val::None);
    }
    let parts: Vec<&str> = s.splitn(6, ':').collect();
    if parts.len() < 6 {
        return Ok(Val::None);
    }
    let (partition, service, region, account, resource) =
        (parts[1], parts[2], parts[3], parts[4], parts[5]);
    if partition.is_empty() || service.is_empty() || resource.is_empty() {
        return Ok(Val::None);
    }
    let mut d = IndexMap::new();
    d.insert("partition".into(), Val::Str(partition.into()));
    d.insert("service".into(), Val::Str(service.into()));
    d.insert("region".into(), Val::Str(region.into()));
    d.insert("accountId".into(), Val::Str(account.into()));
    d.insert(
        "resourceId".into(),
        Val::List(
            resource
                .replace(':', "/")
                .split('/')
                .map(|p| Val::Str(p.to_string()))
                .collect(),
        ),
    );
    Ok(Val::Dict(d))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Val {
        Val::Str(v.into())
    }

    #[test]
    fn templates() {
        let mut scope = Scope::new();
        let mut url = IndexMap::new();
        url.insert("scheme".into(), s("https"));
        scope.insert("url".into(), Val::Dict(url));
        scope.insert("Region".into(), s("us-east-1"));
        scope.insert("Flag".into(), Val::Bool(true));
        assert_eq!(
            resolve_template_string("{url#scheme}://x.{Region}/{Flag}{{a}}", &scope).unwrap(),
            "https://x.us-east-1/True{a}"
        );
        assert!(resolve_template_string("{Missing}", &scope).is_err());
    }

    #[test]
    fn host_labels() {
        let t = Val::Bool(true);
        let f = Val::Bool(false);
        assert!(is_valid_host_label(&s("abc-1"), &f).unwrap());
        assert!(!is_valid_host_label(&s("-abc"), &f).unwrap());
        assert!(!is_valid_host_label(&s("a.b"), &f).unwrap());
        assert!(is_valid_host_label(&s("a.b"), &t).unwrap());
        assert!(is_valid_host_label(&s("abc\n"), &f).unwrap());
        assert!(!is_valid_host_label(&s(&"a".repeat(64)), &f).unwrap());
        assert!(is_virtual_hostable_s3_bucket(&s("abc.bucket"), &t).unwrap());
        assert!(!is_virtual_hostable_s3_bucket(&s("My-bucket"), &f).unwrap());
        assert!(!is_virtual_hostable_s3_bucket(&s("1.2.3.4"), &t).unwrap());
    }

    #[test]
    fn parse_arn_and_attr() {
        let arn = aws_parse_arn(s("arn:aws:s3:us-west-2:123:accesspoint:my/ap")).unwrap();
        assert_eq!(get_attr(arn.clone(), &s("resourceId[2]")).unwrap(), s("ap"));
        assert_eq!(
            get_attr(arn.clone(), &s("resourceId[9]")).unwrap(),
            Val::None
        );
        assert_eq!(get_attr(arn, &s("accountId")).unwrap(), s("123"));
        assert_eq!(aws_parse_arn(s("arn:aws::r:a:")).unwrap(), Val::None);
    }

    #[test]
    fn partition_lookup() {
        let p = aws_partition(s("cn-north-1")).unwrap();
        assert_eq!(get_attr(p, &s("name")).unwrap(), s("aws-cn"));
        let p = aws_partition(s("unknown")).unwrap();
        assert_eq!(get_attr(p, &s("name")).unwrap(), s("aws"));
    }

    #[test]
    fn substrings() {
        let t = Val::Bool(true);
        let f = Val::Bool(false);
        assert_eq!(
            substring(s("abcdef"), Val::Int(0), Val::Int(4), f.clone()).unwrap(),
            s("abcd")
        );
        assert_eq!(
            substring(s("abcdef"), Val::Int(0), Val::Int(4), t).unwrap(),
            s("cdef")
        );
        assert_eq!(
            substring(s("abc"), Val::Int(0), Val::Int(4), f).unwrap(),
            Val::None
        );
    }
}

#[cfg(test)]
mod golden {
    use super::*;

    /// Produced by `RuleSetStandardLibary` of botocore 2.39.3.
    const GOLDEN: &str = r#"{"parse_url": [["https://example.com", {"scheme": "https", "authority": "example.com", "path": "", "normalizedPath": "/", "isIp": false}], ["http://example.com:8080/a/b/../c", {"scheme": "http", "authority": "example.com:8080", "path": "/a/b/../c", "normalizedPath": "/a/c/", "isIp": false}], ["https://1.2.3.4:443/x", {"scheme": "https", "authority": "1.2.3.4:443", "path": "/x", "normalizedPath": "/x/", "isIp": true}], ["https://[::1]:80/", {"scheme": "https", "authority": "[::1]:80", "path": "/", "normalizedPath": "/", "isIp": true}], ["https://example.com/path?x=1", null], ["ftp://x", null], ["https://h:abc/", null], ["https://example.com/a%20b/c d", {"scheme": "https", "authority": "example.com", "path": "/a%20b/c d", "normalizedPath": "/a%2520b/c%20d/", "isIp": false}], ["http://EXAMPLE.com/;p", {"scheme": "http", "authority": "EXAMPLE.com", "path": "/", "normalizedPath": "/", "isIp": false}], ["https://[fe80::1%25en0]/", {"scheme": "https", "authority": "[fe80::1%25en0]", "path": "/", "normalizedPath": "/", "isIp": true}], ["https://user@host:1/x#frag", {"scheme": "https", "authority": "user@host:1", "path": "/x", "normalizedPath": "/x/", "isIp": false}], ["https://example.com/./", {"scheme": "https", "authority": "example.com", "path": "/./", "normalizedPath": "/", "isIp": false}]], "host_label": [["abc", true, true], ["a-b", true, true], ["-ab", false, false], ["ab-", false, false], ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", true, true], ["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", false, false], ["a.b", false, true], ["ab\n", true, true], ["日本", false, false], ["١٢٣", true, true], ["", false, false]], "vhost": [["abc", true, true], ["abc.def", false, true], ["ab.cdef", false, false], ["Abc", false, false], ["1.2.3.4", false, false], ["1.2.3.4\n", false, false], ["a-b-c", true, true], ["abc-", false, false]], "uri": [["a b/c?d", "a%20b%2Fc%3Fd"], ["~-._", "~-._"], ["日本", "%E6%97%A5%E6%9C%AC"]]}"#;

    #[test]
    fn standard_library_matches_botocore() {
        let golden: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        for case in golden["parse_url"].as_array().unwrap() {
            let input = case[0].as_str().unwrap();
            let got = dispatch("parseURL", vec![Val::Str(input.into())]).unwrap();
            assert_eq!(got, Val::from_json(&case[1]), "parseURL({input:?})");
        }
        for case in golden["host_label"].as_array().unwrap() {
            let input = Val::Str(case[0].as_str().unwrap().into());
            for (i, allow) in [false, true].into_iter().enumerate() {
                let got =
                    dispatch("isValidHostLabel", vec![input.clone(), Val::Bool(allow)]).unwrap();
                assert_eq!(
                    got,
                    Val::from_json(&case[i + 1]),
                    "isValidHostLabel({input:?}, {allow})"
                );
            }
        }
        for case in golden["vhost"].as_array().unwrap() {
            let input = Val::Str(case[0].as_str().unwrap().into());
            for (i, allow) in [false, true].into_iter().enumerate() {
                let got = dispatch(
                    "aws.isVirtualHostableS3Bucket",
                    vec![input.clone(), Val::Bool(allow)],
                )
                .unwrap();
                assert_eq!(
                    got,
                    Val::from_json(&case[i + 1]),
                    "vhost({input:?}, {allow})"
                );
            }
        }
        for case in golden["uri"].as_array().unwrap() {
            let got = dispatch(
                "uriEncode",
                vec![Val::Str(case[0].as_str().unwrap().into())],
            )
            .unwrap();
            assert_eq!(got, Val::from_json(&case[1]));
        }
    }
}
