//! `awscli/customizations/cloudformation/yamlhelper.py`: `yaml_parse`
//! (JSON first, then `ruamel.yaml` `YAML(typ='safe', pure=True)` with the
//! CloudFormation intrinsic tags, ordered mappings, and the YAML 1.1
//! boolean resolver) and `yaml_dump`.

use std::collections::HashMap;
use std::mem::MaybeUninit;
use std::sync::LazyLock;

use num_bigint::BigInt;
use regex::Regex;
use unsafe_libyaml as unsafe_sys;

use crate::output::yaml::{DumpOptions, yaml_dump_with};
use crate::value::{Map, Value};

/// Strings starting with this prefix stand for the `datetime.date` /
/// `datetime.datetime` values `construct_yaml_timestamp` returns; the rest
/// is their `isoformat()` (dates) or `isoformat(' ')` (datetimes).
pub(super) const TIMESTAMP_PREFIX: &str = "\u{0}yaml-timestamp:";

/// Whether a parsed value holds a date (which `json.dumps` rejects).
pub(super) fn timestamp_kind(v: &Value) -> Option<&'static str> {
    match v {
        Value::Str(s) => s
            .strip_prefix(TIMESTAMP_PREFIX)
            .map(|t| if t.len() == 10 { "date" } else { "datetime" }),
        Value::List(l) => l.iter().find_map(timestamp_kind),
        Value::Map(m) => m.values().find_map(timestamp_kind),
        _ => None,
    }
}

/// `yaml_dump`.
pub(super) fn yaml_dump(value: &Value) -> String {
    yaml_dump_with(
        value,
        &DumpOptions {
            sort_keys: false,
            yaml11_bools: true,
            timestamp_prefix: Some(TIMESTAMP_PREFIX),
            pure_python: true,
            width: 80,
        },
    )
}

/// `yaml_parse`: the template, or the text of the exception.
pub(super) fn yaml_parse(text: &str) -> Result<Value, String> {
    if let Ok(v) = crate::cli::pyjson::loads(text) {
        return Ok(v);
    }
    let nodes = parse_nodes(text)?;
    match nodes {
        Some(node) => construct(&node),
        None => Ok(Value::Null),
    }
}

#[derive(Debug, Clone)]
enum Node {
    Scalar {
        tag: Option<String>,
        value: String,
        plain: bool,
    },
    Seq {
        tag: Option<String>,
        items: Vec<Node>,
    },
    Map {
        tag: Option<String>,
        pairs: Vec<(Node, Node)>,
    },
}

unsafe fn cstr(p: *const u8) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { std::ffi::CStr::from_ptr(p as *const std::ffi::c_char) };
    Some(s.to_string_lossy().into_owned())
}

/// The libyaml parser error in the `MarkedYAMLError` layout.
unsafe fn parse_error(parser: *const unsafe_sys::yaml_parser_t) -> String {
    let p = unsafe { &*parser };
    let mark = |m: &unsafe_sys::yaml_mark_t| {
        format!(
            "  in \"<unicode string>\", line {}, column {}",
            m.line + 1,
            m.column + 1
        )
    };
    let mut lines = Vec::new();
    if let Some(context) = unsafe { cstr(p.context as *const u8) } {
        lines.push(context);
        lines.push(mark(&p.context_mark));
    }
    if let Some(problem) = unsafe { cstr(p.problem as *const u8) } {
        lines.push(problem);
        lines.push(mark(&p.problem_mark));
    }
    lines.join("\n")
}

/// Compose the first document into nodes (aliases resolved).
fn parse_nodes(text: &str) -> Result<Option<Node>, String> {
    let input = text.as_bytes();
    let mut parser = Box::new(MaybeUninit::<unsafe_sys::yaml_parser_t>::uninit());
    let parser = parser.as_mut_ptr();
    // SAFETY: the parser is initialized before use and deleted at the end;
    // the input outlives it; every event is deleted after it is read.
    unsafe {
        if unsafe_sys::yaml_parser_initialize(parser).fail {
            return Err("cannot initialize the YAML parser".to_string());
        }
        unsafe_sys::yaml_parser_set_encoding(parser, unsafe_sys::YAML_UTF8_ENCODING);
        unsafe_sys::yaml_parser_set_input_string(parser, input.as_ptr(), input.len() as u64);
        let result = compose(parser);
        unsafe_sys::yaml_parser_delete(parser);
        result
    }
}

enum Frame {
    Seq {
        tag: Option<String>,
        anchor: Option<String>,
        items: Vec<Node>,
    },
    Map {
        tag: Option<String>,
        anchor: Option<String>,
        pairs: Vec<(Node, Node)>,
        key: Option<Node>,
    },
}

unsafe fn compose(parser: *mut unsafe_sys::yaml_parser_t) -> Result<Option<Node>, String> {
    let mut anchors: HashMap<String, Node> = HashMap::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut root: Option<Node> = None;
    let mut documents = 0;
    loop {
        let mut event = MaybeUninit::<unsafe_sys::yaml_event_t>::uninit();
        let ev = event.as_mut_ptr();
        if unsafe { unsafe_sys::yaml_parser_parse(parser, ev) }.fail {
            return Err(unsafe { parse_error(parser) });
        }
        let e = unsafe { &*ev };
        let mut done: Option<(Node, Option<String>)> = None;
        let mut finished = false;
        match e.type_ {
            unsafe_sys::YAML_STREAM_END_EVENT => finished = true,
            unsafe_sys::YAML_DOCUMENT_START_EVENT => {
                documents += 1;
                if documents > 1 {
                    unsafe { unsafe_sys::yaml_event_delete(ev) };
                    return Err(
                        "expected a single document in the stream\n  in \"<unicode string>\", line 1, column 1\nbut found another document"
                            .to_string(),
                    );
                }
            }
            unsafe_sys::YAML_ALIAS_EVENT => {
                let name = unsafe { cstr(e.data.alias.anchor) }.unwrap_or_default();
                match anchors.get(&name) {
                    Some(n) => done = Some((n.clone(), None)),
                    None => {
                        unsafe { unsafe_sys::yaml_event_delete(ev) };
                        return Err(format!("found undefined alias {name:?}"));
                    }
                }
            }
            unsafe_sys::YAML_SCALAR_EVENT => {
                let s = unsafe { e.data.scalar };
                let bytes = unsafe { std::slice::from_raw_parts(s.value, s.length as usize) };
                done = Some((
                    Node::Scalar {
                        tag: unsafe { cstr(s.tag) },
                        value: String::from_utf8_lossy(bytes).into_owned(),
                        plain: s.style == unsafe_sys::YAML_PLAIN_SCALAR_STYLE,
                    },
                    unsafe { cstr(s.anchor) },
                ));
            }
            unsafe_sys::YAML_SEQUENCE_START_EVENT => {
                let s = unsafe { e.data.sequence_start };
                stack.push(Frame::Seq {
                    tag: unsafe { cstr(s.tag) },
                    anchor: unsafe { cstr(s.anchor) },
                    items: Vec::new(),
                });
            }
            unsafe_sys::YAML_MAPPING_START_EVENT => {
                let s = unsafe { e.data.mapping_start };
                stack.push(Frame::Map {
                    tag: unsafe { cstr(s.tag) },
                    anchor: unsafe { cstr(s.anchor) },
                    pairs: Vec::new(),
                    key: None,
                });
            }
            unsafe_sys::YAML_SEQUENCE_END_EVENT | unsafe_sys::YAML_MAPPING_END_EVENT => {
                done = match stack.pop() {
                    Some(Frame::Seq { tag, anchor, items }) => {
                        Some((Node::Seq { tag, items }, anchor))
                    }
                    Some(Frame::Map {
                        tag, anchor, pairs, ..
                    }) => Some((Node::Map { tag, pairs }, anchor)),
                    None => None,
                };
            }
            _ => {}
        }
        unsafe { unsafe_sys::yaml_event_delete(ev) };
        if let Some((node, anchor)) = done {
            if let Some(a) = anchor {
                anchors.insert(a, node.clone());
            }
            match stack.last_mut() {
                Some(Frame::Seq { items, .. }) => items.push(node),
                Some(Frame::Map { pairs, key, .. }) => match key.take() {
                    Some(k) => pairs.push((k, node)),
                    None => *key = Some(node),
                },
                None => root = Some(node),
            }
        }
        if finished {
            return Ok(root);
        }
    }
}

const TAG_PREFIX: &str = "tag:yaml.org,2002:";

static BOOL12: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A(?:true|True|TRUE|false|False|FALSE)\n?\z").expect("regex"));
static BOOL11: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\A(?:yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF)\n?\z",
    )
    .expect("regex")
});
static FLOAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\A(?:[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+]?[0-9]+)?|[-+]?(?:[0-9][0-9_]*)(?:[eE][-+]?[0-9]+)|[-+]?\.[0-9_]+(?:[eE][-+][0-9]+)?|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))\n?\z").expect("regex")
});
static INT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\A(?:[-+]?0b[0-1_]+|[-+]?0o?[0-7_]+|[-+]?[0-9_]+|[-+]?0x[0-9a-fA-F_]+)\n?\z")
        .expect("regex")
});
static NULL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A(?:~|null|Null|NULL|)\n?\z").expect("regex"));
static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\A(?:[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]|[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?(?:[Tt]|[ \t]+)[0-9][0-9]?:[0-9][0-9]:[0-9][0-9](?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?)\n?\z").expect("regex")
});
static TIMESTAMP_PARTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([0-9]{4})-([0-9][0-9]?)-([0-9][0-9]?)(?:(?:([Tt])|[ \t]+)([0-9][0-9]?):([0-9][0-9]):([0-9][0-9])(?:\.([0-9]*))?(?:[ \t]*(Z|([-+])([0-9][0-9]?)(?::([0-9][0-9]))?))?)?$").expect("regex")
});

/// `VersionedResolver.resolve` for a plain scalar (YAML 1.2 resolvers plus
/// the YAML 1.1 booleans).
fn resolve(value: &str) -> &'static str {
    let first = value.chars().next();
    let checks: [(&str, &Regex, &'static str); 5] = [
        ("tTfF", &BOOL12, "bool"),
        ("-+0123456789.", &FLOAT, "float"),
        ("-+0123456789", &INT, "int"),
        ("~nN", &NULL, "null"),
        ("0123456789", &TIMESTAMP, "timestamp"),
    ];
    for (chars, re, tag) in checks {
        let applies = match first {
            None => tag == "null",
            Some(c) => chars.contains(c),
        };
        if applies && re.is_match(value) {
            return tag;
        }
    }
    if value == "<<" {
        return "merge";
    }
    if first.is_some_and(|c| "yYnNtTfFoO".contains(c)) && BOOL11.is_match(value) {
        return "bool";
    }
    "str"
}

fn construct_scalar_as(kind: &str, value: &str) -> Result<Value, String> {
    match kind {
        "null" => Ok(Value::Null),
        "bool" => match value.to_lowercase().as_str() {
            "yes" | "y" | "true" | "on" => Ok(Value::Bool(true)),
            "no" | "n" | "false" | "off" => Ok(Value::Bool(false)),
            other => Err(format!("{other:?}")),
        },
        "int" => {
            let s = value.replace('_', "");
            let (sign, s) = match s.strip_prefix('-') {
                Some(r) => (-1, r.to_string()),
                None => (1, s.strip_prefix('+').unwrap_or(&s).to_string()),
            };
            let (digits, radix) = if let Some(r) = s.strip_prefix("0b") {
                (r, 2)
            } else if let Some(r) = s.strip_prefix("0x") {
                (r, 16)
            } else if let Some(r) = s.strip_prefix("0o") {
                (r, 8)
            } else {
                (s.as_str(), 10)
            };
            let n = BigInt::parse_bytes(digits.as_bytes(), radix).ok_or_else(|| {
                format!("invalid literal for int() with base {radix}: '{digits}'")
            })?;
            Ok(Value::Int(n * sign))
        }
        "float" => {
            let s = value.replace('_', "").to_lowercase();
            let (sign, s) = match s.strip_prefix('-') {
                Some(r) => (-1.0, r.to_string()),
                None => (1.0, s.strip_prefix('+').unwrap_or(&s).to_string()),
            };
            let f = match s.as_str() {
                ".inf" => f64::INFINITY,
                ".nan" => return Ok(Value::Float(f64::NAN)),
                other => other
                    .parse::<f64>()
                    .map_err(|_| format!("could not convert string to float: '{other}'"))?,
            };
            Ok(Value::Float(sign * f))
        }
        "timestamp" => timestamp(value),
        _ => Ok(Value::str(value)),
    }
}

/// `create_timestamp` followed by `isoformat()` / `isoformat(' ')`.
fn timestamp(value: &str) -> Result<Value, String> {
    let c = TIMESTAMP_PARTS
        .captures(value)
        .ok_or_else(|| format!("failed to construct timestamp from \"{value}\""))?;
    let num = |i: usize| c.get(i).map(|m| m.as_str().parse::<u32>().unwrap_or(0));
    let (y, mo, d) = (
        num(1).unwrap_or(0),
        num(2).unwrap_or(0),
        num(3).unwrap_or(0),
    );
    let date = format!("{y:04}-{mo:02}-{d:02}");
    let Some(h) = num(5) else {
        return Ok(Value::Str(format!("{TIMESTAMP_PREFIX}{date}")));
    };
    let (mi, s) = (num(6).unwrap_or(0), num(7).unwrap_or(0));
    let mut out = format!("{date} {h:02}:{mi:02}:{s:02}");
    if let Some(f) = c.get(8).map(|m| m.as_str()).filter(|f| !f.is_empty()) {
        let mut frac: u32 = format!("{:0<6}", &f[..f.len().min(6)]).parse().unwrap_or(0);
        if f.len() > 6 && f.as_bytes()[6] > b'4' {
            frac += 1;
        }
        if frac > 999_999 {
            frac = 0;
        }
        if frac != 0 {
            out.push_str(&format!(".{frac:06}"));
        }
    }
    match (c.get(9).map(|m| m.as_str()), c.get(10)) {
        (Some("Z"), _) => out.push_str("+00:00"),
        (Some(_), Some(sign)) => {
            let th = num(11).unwrap_or(0);
            let tm = num(12).unwrap_or(0);
            out.push_str(&format!("{}{th:02}:{tm:02}", sign.as_str()));
        }
        _ => {}
    }
    Ok(Value::Str(format!("{TIMESTAMP_PREFIX}{out}")))
}

fn key_string(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => crate::pyfmt::py_str(other),
    }
}

/// `flatten_mapping` then the ordered pairs.
fn flatten(pairs: &[(Node, Node)]) -> Result<Vec<(Node, Node)>, String> {
    let mut merge: Vec<(Node, Node)> = Vec::new();
    let mut own = Vec::new();
    for (k, v) in pairs {
        let is_merge = matches!(k, Node::Scalar { tag: None, value, plain: true } if value == "<<")
            || matches!(k, Node::Scalar { tag: Some(t), .. } if t == "tag:yaml.org,2002:merge");
        if is_merge {
            match v {
                Node::Map { pairs, .. } => merge.extend(flatten(pairs)?),
                Node::Seq { items, .. } => {
                    let mut sub = Vec::new();
                    for item in items {
                        match item {
                            Node::Map { pairs, .. } => sub.push(flatten(pairs)?),
                            _ => {
                                return Err(
                                    "while constructing a mapping\nexpected a mapping for merging"
                                        .to_string(),
                                );
                            }
                        }
                    }
                    sub.reverse();
                    for s in sub {
                        merge.extend(s);
                    }
                }
                _ => {
                    return Err("while constructing a mapping\nexpected a mapping or list of mappings for merging".to_string());
                }
            }
        } else {
            own.push((k.clone(), v.clone()));
        }
    }
    merge.extend(own);
    Ok(merge)
}

fn construct_map(pairs: &[(Node, Node)]) -> Result<Value, String> {
    let mut m = Map::new();
    for (k, v) in flatten(pairs)? {
        let key = key_string(&construct(&k)?);
        let value = construct(&v)?;
        m.insert(key, value);
    }
    Ok(Value::Map(m))
}

/// `intrinsics_multi_constructor`.
fn intrinsic(tag: &str, node: &Node) -> Result<Value, String> {
    let tag = &tag[1..];
    let prefix = if matches!(tag, "Ref" | "Condition") {
        ""
    } else {
        "Fn::"
    };
    let value = match node {
        Node::Scalar { value, .. } if tag == "GetAtt" => {
            let parts: Vec<Value> = match value.split_once('.') {
                Some((a, b)) => vec![Value::str(a), Value::str(b)],
                None => vec![Value::str(value.as_str())],
            };
            Value::List(parts)
        }
        Node::Scalar { value, .. } => Value::str(value.as_str()),
        Node::Seq { items, .. } => {
            Value::List(items.iter().map(construct).collect::<Result<_, _>>()?)
        }
        Node::Map { pairs, .. } => {
            let mut m = Map::new();
            for (k, v) in pairs {
                m.insert(key_string(&construct(k)?), construct(v)?);
            }
            Value::Map(m)
        }
    };
    let mut m = Map::new();
    m.insert(format!("{prefix}{tag}"), value);
    Ok(Value::Map(m))
}

fn construct(node: &Node) -> Result<Value, String> {
    let tag = match node {
        Node::Scalar { tag, .. } | Node::Seq { tag, .. } | Node::Map { tag, .. } => tag.as_deref(),
    };
    if let Some(t) = tag
        && t.starts_with('!')
    {
        return intrinsic(t, node);
    }
    match node {
        Node::Scalar { tag, value, plain } => match tag.as_deref() {
            Some(t) => match t.strip_prefix(TAG_PREFIX) {
                Some(kind @ ("null" | "bool" | "int" | "float" | "timestamp" | "str")) => {
                    construct_scalar_as(kind, value)
                }
                _ => Err(format!(
                    "could not determine a constructor for the tag '{t}'"
                )),
            },
            None if *plain => construct_scalar_as(resolve(value), value),
            None => Ok(Value::str(value.as_str())),
        },
        Node::Seq { items, .. } => Ok(Value::List(
            items.iter().map(construct).collect::<Result<_, _>>()?,
        )),
        Node::Map { pairs, .. } => construct_map(pairs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intrinsics_and_scalars() {
        let v = yaml_parse(
            "A: !Ref X\nB: !GetAtt R.Arn\nC: !Sub [a, {b: 1}]\nD: yes\nE: 0x10\nF: 2010-09-09\nG: '1'\nH: ~\nI: 1.5\n",
        )
        .unwrap();
        let m = v.as_map().unwrap();
        assert_eq!(m["A"].get("Ref"), Some(&Value::str("X")));
        assert_eq!(
            m["B"].get("Fn::GetAtt"),
            Some(&Value::List(vec![Value::str("R"), Value::str("Arn")]))
        );
        assert_eq!(m["D"], Value::Bool(true));
        assert_eq!(m["E"], Value::int(16));
        assert_eq!(timestamp_kind(&m["F"]), Some("date"));
        assert_eq!(m["G"], Value::str("1"));
        assert_eq!(m["H"], Value::Null);
        assert_eq!(m["I"], Value::Float(1.5));
        assert_eq!(
            yaml_dump(&v),
            "A:\n  Ref: X\nB:\n  Fn::GetAtt:\n  - R\n  - Arn\nC:\n  Fn::Sub:\n  - a\n  - b: 1\nD: true\nE: 16\nF: 2010-09-09\nG: '1'\nH: null\nI: 1.5\n"
        );
    }

    #[test]
    fn anchors_and_merge() {
        let v = yaml_parse("a: &x {k: 1, j: 2}\nb:\n  <<: *x\n  k: 3\n").unwrap();
        let b = v.get("b").unwrap().as_map().unwrap();
        assert_eq!(b.keys().collect::<Vec<_>>(), vec!["k", "j"]);
        assert_eq!(b["k"], Value::int(3));
    }

    #[test]
    fn yes_string_is_quoted() {
        let mut m = Map::new();
        m.insert("a".into(), Value::str("yes"));
        m.insert("b".into(), Value::str("on"));
        assert_eq!(yaml_dump(&Value::Map(m)), "a: 'yes'\nb: 'on'\n");
    }
}
