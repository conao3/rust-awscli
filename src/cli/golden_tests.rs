//! Command lines compared with the reference AWS CLI (`goldens.rs`).

use std::collections::HashMap;

use super::goldens::{FIXTURES, GOLDENS};
use super::{Command, capture_warnings, parse_command, parse_globals_with_config};
use crate::value::Value;

pub(super) struct Golden {
    pub args: &'static [&'static str],
    pub env: &'static [(&'static str, &'static str)],
    pub rc: i32,
    pub stdout: &'static str,
    pub stderr: &'static str,
    pub record: Option<&'static str>,
}

/// The encoding `ref_harness.py` uses for parameter values.
fn enc(v: &Value) -> serde_json::Value {
    use serde_json::json;
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => json!(b),
        Value::Int(i) => json!({"__int__": i.to_string()}),
        Value::Float(f) => json!({"__float__": crate::pyfmt::float_repr(*f)}),
        Value::Str(s) => json!(s),
        Value::Bytes(b) => json!({"__bytes__": hex::encode(b)}),
        Value::List(l) => serde_json::Value::Array(l.iter().map(enc).collect()),
        Value::Map(m) => {
            serde_json::Value::Object(m.iter().map(|(k, v)| (k.clone(), enc(v))).collect())
        }
    }
}

fn record(cmd: &Command) -> Option<String> {
    use serde_json::json;
    let v = match cmd {
        Command::Operation(inv) if inv.stub_response.is_some() => {
            json!({"kind": "stub", "params": enc(&inv.params)})
        }
        Command::Operation(inv) => json!({
            "kind": "call",
            "operation": inv.operation,
            "paginate": inv.paginate,
            "max_items": inv.pagination.max_items,
            "page_size": inv.pagination.page_size,
            "starting_token": inv.pagination.starting_token,
            "params": enc(&inv.params),
        }),
        Command::Wait(w) => json!({"kind": "wait", "waiter": w.waiter, "params": enc(&w.params)}),
        _ => return None,
    };
    Some(serde_json::to_string(&v).expect("json"))
}

fn run(golden: &Golden, dir: &str) -> (i32, String, String, Option<String>) {
    let args: Vec<String> = golden
        .args
        .iter()
        .map(|a| a.replace("@DIR@", dir))
        .collect();
    let mut config: HashMap<String, String> = HashMap::new();
    for (k, v) in golden.env {
        if *k == "AWS_CLI_ERROR_FORMAT" {
            config.insert("cli_error_format".to_string(), v.to_string());
        }
    }
    let (result, warnings) = capture_warnings(|| {
        parse_globals_with_config(&args, &config)
            .and_then(|(g, rest)| parse_command(&rest, &g, &config))
    });
    let mut stderr = warnings.concat();
    match result {
        Ok(cmd) => {
            let stdout = match &cmd {
                Command::Print(s) => s.clone(),
                Command::Version => "<version>".to_string(),
                _ => String::new(),
            };
            (0, stdout, stderr, record(&cmd))
        }
        Err(e) => {
            stderr.push_str(&e.message);
            (e.code as i32, String::new(), stderr, None)
        }
    }
}

/// Goldens that depend on work outside this module: the message of a
/// `--query` compile error comes from `crate::jmes`, and the `Expires`
/// member of `s3.PutObject` is a timestamp only after
/// `service-2.sdk-extras.json` is merged into the embedded model.
const PENDING: &[&[&str]] = &[
    &["--query", "a[", "sts", "get-caller-identity"],
    &["s3api", "put-object", "--generate-cli-skeleton"],
    &[
        "s3api",
        "put-object",
        "--generate-cli-skeleton",
        "--bucket",
        "b",
    ],
];

#[test]
fn command_lines_match_the_reference() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().join("fx");
    std::fs::create_dir(&dir).expect("fixture dir");
    for (name, data) in FIXTURES {
        std::fs::write(dir.join(name), data).expect("fixture");
    }
    let dir = dir.to_str().expect("utf-8 path").to_string();
    let mut failures = Vec::new();
    for golden in GOLDENS {
        if PENDING.contains(&golden.args) {
            continue;
        }
        let (rc, stdout, stderr, rec) = run(golden, &dir);
        let expected_stdout = if golden.stdout.starts_with("aws-cli/") {
            "<version>".to_string()
        } else {
            golden.stdout.replace("@DIR@", &dir)
        };
        let expected_record = golden.record.map(|r| {
            let v: serde_json::Value =
                serde_json::from_str(&r.replace("@DIR@", &dir)).expect("record json");
            serde_json::to_string(&v).expect("json")
        });
        let expected_stderr = golden.stderr.replace("@DIR@", &dir);
        if rc != golden.rc
            || stdout != expected_stdout
            || stderr != expected_stderr
            || rec != expected_record
        {
            failures.push(format!(
                "{:?}\n  rc {} vs {}\n  stdout {:?}\n      vs {:?}\n  stderr {:?}\n      vs {:?}\n  record {:?}\n      vs {:?}",
                golden.args, golden.rc, rc, expected_stdout, stdout, expected_stderr, stderr, expected_record, rec
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
