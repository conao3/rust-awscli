//! The dynamic value used for parameters, responses, and output.
//!
//! It mirrors the Python values botocore and the AWS CLI work with
//! (`dict`, `list`, `str`, `int`, `float`, `bool`, `None`, `bytes`), so that
//! formatting rules that depend on the Python type can be ported exactly.

use indexmap::IndexMap;
use num_bigint::BigInt;

pub type Map = IndexMap<String, Value>;

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(BigInt),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Map(Map),
}

impl Value {
    pub fn map() -> Value {
        Value::Map(Map::new())
    }

    pub fn int(v: i64) -> Value {
        Value::Int(BigInt::from(v))
    }

    pub fn str(v: impl Into<String>) -> Value {
        Value::Str(v.into())
    }

    pub fn as_map(&self) -> Option<&Map> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_map_mut(&mut self) -> Option<&mut Map> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&Vec<Value>> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => num_traits::ToPrimitive::to_i64(i),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_map().and_then(|m| m.get(key))
    }

    pub fn is_scalar(&self) -> bool {
        !matches!(self, Value::List(_) | Value::Map(_))
    }

    /// Python truthiness.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(i) => i != &BigInt::from(0),
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Bytes(b) => !b.is_empty(),
            Value::List(l) => !l.is_empty(),
            Value::Map(m) => !m.is_empty(),
        }
    }

    /// Convert a `serde_json::Value` the way Python's `json.loads` does:
    /// a number with a fraction or exponent becomes a float, any other
    /// number an arbitrary-precision int.
    pub fn from_json(v: &serde_json::Value) -> Value {
        match v {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => number_from_lexeme(&n.to_string()),
            serde_json::Value::String(s) => Value::Str(s.clone()),
            serde_json::Value::Array(a) => Value::List(a.iter().map(Value::from_json).collect()),
            serde_json::Value::Object(o) => Value::Map(
                o.iter()
                    .map(|(k, v)| (k.clone(), Value::from_json(v)))
                    .collect(),
            ),
        }
    }

    /// Parse JSON text with Python `json.loads` semantics (including the
    /// `NaN`, `Infinity`, and `-Infinity` literals).
    pub fn parse_json(text: &str) -> Result<Value, serde_json::Error> {
        match serde_json::from_str::<serde_json::Value>(text) {
            Ok(v) => Ok(Value::from_json(&v)),
            Err(e) => {
                if text.contains("NaN") || text.contains("Infinity") {
                    let replaced = replace_non_finite(text);
                    let v: serde_json::Value = serde_json::from_str(&replaced)?;
                    Ok(restore_non_finite(Value::from_json(&v)))
                } else {
                    Err(e)
                }
            }
        }
    }
}

const NAN_MARK: &str = "\u{0}awsr-nan\u{0}";
const INF_MARK: &str = "\u{0}awsr-inf\u{0}";
const NINF_MARK: &str = "\u{0}awsr-ninf\u{0}";
const MARK_JSON_NAN: &str = r#""\u0000awsr-nan\u0000""#;
const MARK_JSON_INF: &str = r#""\u0000awsr-inf\u0000""#;
const MARK_JSON_NINF: &str = r#""\u0000awsr-ninf\u0000""#;

/// Replace bare `NaN` / `Infinity` / `-Infinity` tokens outside strings with
/// marker strings so serde_json accepts the document.
fn replace_non_finite(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            let ch = text[i..].chars().next().unwrap();
            out.push(ch);
            if ch == '\\'
                && let Some(next) = text[i + 1..].chars().next()
            {
                out.push(next);
                i += 1 + next.len_utf8();
                continue;
            }
            if ch == '"' {
                in_str = false;
            }
            i += ch.len_utf8();
            continue;
        }
        if c == b'"' {
            in_str = true;
            out.push('"');
            i += 1;
            continue;
        }
        let rest = &text[i..];
        if rest.starts_with("NaN") {
            out.push_str(MARK_JSON_NAN);
            i += 3;
        } else if rest.starts_with("-Infinity") {
            out.push_str(MARK_JSON_NINF);
            i += 9;
        } else if rest.starts_with("Infinity") {
            out.push_str(MARK_JSON_INF);
            i += 8;
        } else {
            let ch = rest.chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn restore_non_finite(v: Value) -> Value {
    match v {
        Value::Str(s) if s == NAN_MARK => Value::Float(f64::NAN),
        Value::Str(s) if s == INF_MARK => Value::Float(f64::INFINITY),
        Value::Str(s) if s == NINF_MARK => Value::Float(f64::NEG_INFINITY),
        Value::List(l) => Value::List(l.into_iter().map(restore_non_finite).collect()),
        Value::Map(m) => Value::Map(
            m.into_iter()
                .map(|(k, v)| (k, restore_non_finite(v)))
                .collect(),
        ),
        other => other,
    }
}

/// Classify a JSON number lexeme like Python's `json.loads`.
pub fn number_from_lexeme(lexeme: &str) -> Value {
    if lexeme.contains(['.', 'e', 'E']) {
        Value::Float(lexeme.parse::<f64>().unwrap_or(f64::NAN))
    } else {
        match lexeme.parse::<BigInt>() {
            Ok(i) => Value::Int(i),
            Err(_) => Value::Float(lexeme.parse::<f64>().unwrap_or(f64::NAN)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_numbers_follow_python_types() {
        let v = Value::parse_json(
            r#"{"a": 1, "b": 1.50, "c": 1e2, "d": 123456789012345678901234567890}"#,
        )
        .unwrap();
        assert_eq!(v.get("a"), Some(&Value::int(1)));
        assert_eq!(v.get("b"), Some(&Value::Float(1.5)));
        assert_eq!(v.get("c"), Some(&Value::Float(100.0)));
        assert!(matches!(v.get("d"), Some(Value::Int(_))));
    }

    #[test]
    fn json_non_finite_literals() {
        let v = Value::parse_json(r#"{"a": NaN, "b": -Infinity, "c": "NaN"}"#).unwrap();
        assert!(matches!(v.get("a"), Some(Value::Float(f)) if f.is_nan()));
        assert_eq!(v.get("b"), Some(&Value::Float(f64::NEG_INFINITY)));
        assert_eq!(v.get("c"), Some(&Value::str("NaN")));
    }
}
