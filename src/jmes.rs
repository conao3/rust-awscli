//! JMESPath evaluation over [`Value`], used by `--query`, pagination, and
//! waiters.

use anyhow::{Result, anyhow};

use crate::pyfmt;
use crate::value::{Map, Value};

/// A compiled JMESPath expression.
pub struct Expression {
    source: String,
    inner: jmespath::Expression<'static>,
}

impl Expression {
    pub fn compile(source: &str) -> Result<Expression> {
        let inner = jmespath::compile(source).map_err(|e| anyhow!("{e}"))?;
        Ok(Expression {
            source: source.to_string(),
            inner,
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn search(&self, data: &Value) -> Result<Value> {
        let json = to_json(data);
        let result = self.inner.search(json).map_err(|e| anyhow!("{e}"))?;
        let json = serde_json::to_value(&*result)?;
        Ok(Value::from_json(&json))
    }
}

pub fn search(expression: &str, data: &Value) -> Result<Value> {
    Expression::compile(expression)?.search(data)
}

/// Convert to JSON, keeping Python's int / float distinction in the number
/// lexeme so [`Value::from_json`] restores the same type.
pub fn to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::Number(
            serde_json::from_str::<serde_json::Number>(&i.to_string()).expect("int lexeme"),
        ),
        Value::Float(f) if f.is_finite() => serde_json::Value::Number(
            serde_json::from_str::<serde_json::Number>(&pyfmt::float_repr(*f))
                .expect("float lexeme"),
        ),
        Value::Float(f) => serde_json::Value::String(pyfmt::float_repr(*f)),
        Value::Str(s) => serde_json::Value::String(s.clone()),
        Value::Bytes(b) => serde_json::Value::String(String::from_utf8_lossy(b).into_owned()),
        Value::List(l) => serde_json::Value::Array(l.iter().map(to_json).collect()),
        Value::Map(m) => {
            serde_json::Value::Object(m.iter().map(|(k, v)| (k.clone(), to_json(v))).collect())
        }
    }
}

/// botocore `utils.set_value_from_jmespath`: assign `value` at a dotted
/// key path, creating intermediate dicts.
pub fn set_value_from_jmespath(source: &mut Map, expression: &str, value: Value) {
    match expression.split_once('.') {
        Some((head, rest)) => {
            let entry = source.entry(head.to_string()).or_insert_with(Value::map);
            if !matches!(entry, Value::Map(_)) {
                *entry = Value::map();
            }
            if let Value::Map(m) = entry {
                set_value_from_jmespath(m, rest, value);
            }
        }
        None => {
            source.insert(expression.to_string(), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_keeps_number_types() {
        let v = Value::parse_json(r#"{"a": [{"b": 1}, {"b": 2.5}]}"#).unwrap();
        let r = search("a[].b", &v).unwrap();
        assert_eq!(r, Value::List(vec![Value::int(1), Value::Float(2.5)]));
    }
}
