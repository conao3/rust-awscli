//! `--generate-cli-skeleton` (`awscli/customizations/generatecliskeleton.py`,
//! `botocore/utils.py` `ArgumentGenerator`).

use crate::model::Shape;
use crate::value::{Map, Value};

/// `ArgumentGenerator(use_member_names).generate_skeleton(shape)`.
pub(crate) fn generate(shape: &Shape<'_>, use_member_names: bool) -> Value {
    let mut stack = Vec::new();
    gen_skeleton(shape, &mut stack, "", use_member_names)
}

fn gen_skeleton(shape: &Shape<'_>, stack: &mut Vec<String>, name: &str, names: bool) -> Value {
    stack.push(shape.name.to_string());
    let v = match shape.type_name() {
        "structure" => {
            let mut skeleton = Map::new();
            if stack.iter().filter(|n| *n == shape.name).count() <= 1 {
                for (member, member_shape) in shape.members() {
                    let v = gen_skeleton(&member_shape, stack, member, names);
                    skeleton.insert(member.to_string(), v);
                }
            }
            Value::Map(skeleton)
        }
        "list" => {
            let member = shape.list_member().expect("list member");
            let member_name = if names {
                member.name.to_string()
            } else {
                String::new()
            };
            Value::List(vec![gen_skeleton(&member, stack, &member_name, names)])
        }
        "map" => {
            let value = shape.map_value().expect("map value");
            let mut m = Map::new();
            m.insert(
                "KeyName".to_string(),
                gen_skeleton(&value, stack, "", names),
            );
            Value::Map(m)
        }
        "string" => {
            if names {
                Value::str(name)
            } else {
                match enum_values(shape).first() {
                    Some(e) => Value::str(*e),
                    None => Value::str(""),
                }
            }
        }
        "integer" | "long" => Value::int(0),
        "float" | "double" => Value::Float(0.0),
        "boolean" => Value::Bool(true),
        "timestamp" => Value::str("1970-01-01T00:00:00"),
        _ => Value::Null,
    };
    stack.pop();
    v
}

/// `json.dump(skeleton, indent=4, default=json_encoder)` followed by a
/// newline (`ensure_ascii=True`).
pub(crate) fn input_json(shape: Option<&Shape<'_>>) -> String {
    let skeleton = match shape {
        Some(s) => generate(s, false),
        None => Value::map(),
    };
    let mut out = ensure_ascii(&crate::pyfmt::json_dumps_pretty(&skeleton));
    out.push('\n');
    out
}

/// Escape non-ASCII characters the way `json.dumps(ensure_ascii=True)`
/// does (they only occur inside strings).
fn ensure_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// `StringShape.enum`, which botocore takes from the member reference
/// when it carries one.
pub(crate) fn enum_values<'a>(shape: &Shape<'a>) -> Vec<&'a str> {
    shape
        .get("enum")
        .and_then(crate::bjson::Node::as_array)
        .map(|v| v.iter().filter_map(crate::bjson::Node::as_str).collect())
        .unwrap_or_default()
}
