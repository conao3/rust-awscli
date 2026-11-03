//! The jmespath.py 1.0.1 compliance suite (`tests/compliance/*.json` and
//! `tests/legacy/*.json`, run like `tests/test_compliance.py`) and goldens
//! produced by running jmespath.py 1.0.1 over the same cases plus extra
//! ones (`tests/jmespath-compliance/generate_goldens.py`).

use std::path::{Path, PathBuf};

use awsr::jmes::{self, Expression};
use awsr::pyfmt;
use awsr::value::Value;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/jmespath-compliance")
}

fn load(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).expect("read test file");
    Value::parse_json(&text).expect("valid JSON")
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).expect("string field")
}

fn run(given: &Value, expression: &str) -> anyhow::Result<Value> {
    Expression::compile(expression)?.search(given)
}

/// Python `==` as `test_compliance.py` uses it (`1 == 1.0`).
fn loose_eq(a: &Value, b: &Value) -> bool {
    let num = |v: &Value| match v {
        Value::Int(i) => num_traits::ToPrimitive::to_f64(i),
        Value::Float(f) => Some(*f),
        _ => None,
    };
    match (a, b) {
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| loose_eq(p, q))
        }
        (Value::Map(x), Value::Map(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| loose_eq(v, w)))
        }
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => x == y,
            _ => a == b,
        },
    }
}

#[test]
fn compliance_suite() {
    let mut files = Vec::new();
    for sub in ["compliance", "legacy"] {
        for entry in std::fs::read_dir(dir().join(sub)).expect("compliance dir") {
            files.push(entry.expect("entry").path());
        }
    }
    files.sort();
    let mut failures = Vec::new();
    let mut count = 0;
    for file in &files {
        let suites = load(file);
        for suite in suites.as_list().expect("suite list") {
            let given = suite.get("given").expect("given");
            for case in suite.get("cases").and_then(Value::as_list).expect("cases") {
                let expression = str_field(case, "expression");
                let actual = run(given, expression);
                let name = file.file_name().unwrap().to_string_lossy();
                if let Some(expected) = case.get("result") {
                    count += 1;
                    match actual {
                        Ok(v) if loose_eq(&v, expected) => {}
                        other => failures.push(format!(
                            "{name}: {expression:?}: expected {}, got {other:?}",
                            pyfmt::json_dumps_default(expected)
                        )),
                    }
                } else if case.get("error").is_some() {
                    count += 1;
                    let ok = match &actual {
                        Err(e) => e
                            .downcast_ref::<jmes::Error>()
                            .is_some_and(|e| e.is_value_error()),
                        Ok(_) => false,
                    };
                    if !ok {
                        failures.push(format!(
                            "{name}: {expression:?}: expected a ValueError, got {actual:?}"
                        ));
                    }
                } else {
                    // Benchmarks: only require evaluation not to panic.
                    let _ = actual;
                }
            }
        }
    }
    assert!(count > 800, "only {count} compliance cases");
    assert!(
        failures.is_empty(),
        "{} of {count} compliance cases fail:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Drop the `at 0x...` address of an object `repr`.
fn without_address(s: &str) -> String {
    match s.find(" object at 0x") {
        Some(i) => {
            let rest = &s[i + " object at 0x".len()..];
            let end = rest
                .find(|c: char| !c.is_ascii_hexdigit())
                .unwrap_or(rest.len());
            format!("{} object{}", &s[..i], &rest[end..])
        }
        None => s.to_string(),
    }
}

#[test]
fn python_goldens() {
    let cases = load(&dir().join("goldens.json"));
    let cases = cases.as_list().expect("golden list");
    let mut failures = Vec::new();
    for case in cases {
        let given = case.get("given").expect("given");
        let expression = str_field(case, "expression");
        let source = str_field(case, "source");
        let actual = run(given, expression);
        if let Some(expected) = case.get("result").and_then(Value::as_str) {
            match &actual {
                Ok(v) if pyfmt::json_dumps_default(v) == expected => {}
                Ok(v) => failures.push(format!(
                    "{source}: {expression:?}\n  expected {expected}\n  actual   {}",
                    pyfmt::json_dumps_default(v)
                )),
                Err(e) => failures.push(format!(
                    "{source}: {expression:?}\n  expected {expected}\n  error    {e}"
                )),
            }
        } else {
            let class = str_field(case, "error_class");
            let message = without_address(str_field(case, "error"));
            match &actual {
                Err(e) => {
                    let e = e.downcast_ref::<jmes::Error>().expect("jmes::Error");
                    if e.class() != class || e.to_string() != message {
                        failures.push(format!(
                            "{source}: {expression:?}\n  expected {class}: {message}\n  actual   {}: {e}",
                            e.class()
                        ));
                    }
                }
                Ok(v) => failures.push(format!(
                    "{source}: {expression:?}\n  expected {class}: {message}\n  result   {}",
                    pyfmt::json_dumps_default(v)
                )),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} goldens differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}
