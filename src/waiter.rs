//! Waiters (`botocore/waiter.py`, `awscli/customizations/waiters.py`).

use std::time::Duration;

use anyhow::Result;

use crate::paginate::{Expr, py_eq};
use crate::pyfmt;
use crate::value::Value;

pub enum WaitOutcome {
    Success,
    /// `Waiter <name> failed: <reason>` (botocore `WaiterError`).
    Failed(String),
}

/// The exit status the AWS CLI returns when a waiter fails: `WaiterError`
/// is a `BotoCoreError`, handled by `GeneralExceptionHandler`
/// (`awscli/errorhandler.py`).
pub const WAITER_FAILURE_EXIT_CODE: i32 = 255;

impl WaitOutcome {
    /// The process exit status (`WaiterCaller.invoke` returns 0 on
    /// success).
    pub fn exit_code(&self) -> i32 {
        match self {
            WaitOutcome::Success => 0,
            WaitOutcome::Failed(_) => WAITER_FAILURE_EXIT_CODE,
        }
    }

    /// What the AWS CLI writes to stderr (`errorformat.write_error`):
    /// `"\naws: [ERROR]: Waiter <name> failed: <reason>\n"`.
    pub fn stderr(&self) -> Option<String> {
        match self {
            WaitOutcome::Success => None,
            WaitOutcome::Failed(msg) => Some(format!("\naws: [ERROR]: {}\n", msg.trim())),
        }
    }
}

/// botocore `WaiterConfigError`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("Error processing waiter config: {0}")]
pub struct WaiterConfigError(pub String);

/// One poll: returns the parsed response, or the parsed error response
/// (`{"Error": {"Code": ...}}`) for service errors.
pub type PollFn<'a> = dyn FnMut(&Value) -> Result<Value> + 'a;

enum Matcher {
    Path(Expr),
    PathAll(Expr),
    PathAny(Expr),
    Status,
    Error,
}

/// botocore `AcceptorConfig`.
struct Acceptor {
    state: String,
    matcher_name: String,
    expected: Value,
    argument: Option<String>,
    matcher: Matcher,
}

/// botocore `is_valid_waiter_error`.
fn is_valid_waiter_error(response: &Value) -> bool {
    matches!(response.get("Error"), Some(Value::Map(e)) if e.contains_key("Code"))
}

impl Acceptor {
    fn new(config: &serde_json::Value) -> Result<Acceptor> {
        let field = |k: &str| {
            config
                .get(k)
                .ok_or_else(|| anyhow::anyhow!("{}", pyfmt::str_repr(k)))
        };
        let state = py_str_json(field("state")?);
        let matcher_name = py_str_json(field("matcher")?);
        let expected = Value::from_json(field("expected")?);
        let argument = config.get("argument").map(py_str_json);
        let compile = || Expr::compile(argument.as_deref().unwrap_or_default());
        let matcher = match matcher_name.as_str() {
            "path" => Matcher::Path(compile()?),
            "pathAll" => Matcher::PathAll(compile()?),
            "pathAny" => Matcher::PathAny(compile()?),
            "status" => Matcher::Status,
            "error" => Matcher::Error,
            other => return Err(WaiterConfigError(format!("Unknown acceptor: {other}")).into()),
        };
        Ok(Acceptor {
            state,
            matcher_name,
            expected,
            argument,
            matcher,
        })
    }

    /// `AcceptorConfig.explanation`.
    fn explanation(&self) -> String {
        let argument = self.argument.as_deref().unwrap_or("None");
        let expected = pyfmt::py_str(&self.expected);
        match self.matcher_name.as_str() {
            "path" => {
                format!("For expression \"{argument}\" we matched expected path: \"{expected}\"")
            }
            "pathAll" => format!(
                "For expression \"{argument}\" all members matched excepted path: \"{expected}\""
            ),
            "pathAny" => format!(
                "For expression \"{argument}\" we matched expected path: \"{expected}\" at least once"
            ),
            "status" => format!("Matched expected HTTP status code: {expected}"),
            "error" => format!("Matched expected service error code: {expected}"),
            other => format!("No explanation for unknown waiter type: \"{other}\""),
        }
    }

    /// The acceptor's `matcher_func` (a falsy result counts as no match).
    fn matches(&self, response: &Value) -> Result<bool> {
        match &self.matcher {
            Matcher::Path(expr) => {
                if is_valid_waiter_error(response) {
                    return Ok(false);
                }
                Ok(py_eq(&expr.search(response)?, &self.expected))
            }
            Matcher::PathAll(expr) | Matcher::PathAny(expr) => {
                if is_valid_waiter_error(response) {
                    return Ok(false);
                }
                let result = expr.search(response)?;
                let Value::List(items) = result else {
                    return Ok(false);
                };
                if items.is_empty() {
                    return Ok(false);
                }
                if matches!(self.matcher, Matcher::PathAll(_)) {
                    Ok(items.iter().all(|e| py_eq(e, &self.expected)))
                } else {
                    Ok(items.iter().any(|e| py_eq(e, &self.expected)))
                }
            }
            Matcher::Status => {
                let status = response
                    .get("ResponseMetadata")
                    .and_then(|m| m.get("HTTPStatusCode"))
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(py_eq(&status, &self.expected))
            }
            Matcher::Error => {
                let error = response.get("Error");
                match &self.expected {
                    Value::Bool(true) => Ok(match error {
                        Some(Value::Map(e)) => e.contains_key("Code"),
                        Some(Value::Str(s)) => s.contains("Code"),
                        Some(Value::List(l)) => l.contains(&Value::str("Code")),
                        _ => false,
                    }),
                    Value::Bool(false) => Ok(error.is_none()),
                    expected => {
                        let code = match error {
                            None => Value::str(""),
                            Some(e) => e.get("Code").cloned().unwrap_or(Value::str("")),
                        };
                        Ok(py_eq(&code, expected))
                    }
                }
            }
        }
    }
}

fn py_str_json(v: &serde_json::Value) -> String {
    pyfmt::py_str(&Value::from_json(v))
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => num_traits::ToPrimitive::to_f64(i),
        Value::Float(f) => Some(*f),
        Value::Bool(b) => Some(*b as i64 as f64),
        _ => None,
    }
}

/// Run the waiter `name` (`config` is its entry in `waiters-2.json`)
/// until it succeeds, fails, or runs out of attempts (`Waiter.wait`, with
/// the model's `delay` and `maxAttempts` as the AWS CLI never passes a
/// `WaiterConfig`).
pub async fn wait(
    name: &str,
    config: &serde_json::Map<String, serde_json::Value>,
    params: &Value,
    poll: &mut PollFn<'_>,
) -> Result<WaitOutcome> {
    let field = |k: &str| {
        config
            .get(k)
            .map(Value::from_json)
            .ok_or_else(|| anyhow::anyhow!("{}", pyfmt::str_repr(k)))
    };
    field("operation")?;
    let delay = field("delay")?;
    let max_attempts = field("maxAttempts")?;
    let acceptors = config
        .get("acceptors")
        .and_then(|a| a.as_array())
        .ok_or_else(|| anyhow::anyhow!("'acceptors'"))?
        .iter()
        .map(Acceptor::new)
        .collect::<Result<Vec<_>>>()?;
    let sleep = number(&delay)
        .ok_or_else(|| anyhow::anyhow!("'{}' object cannot be interpreted as an integer", "str"))?;
    let max_attempts = number(&max_attempts).unwrap_or(f64::INFINITY);
    let mut current_state = "waiting".to_string();
    let mut last_matched = false;
    let mut num_attempts = 0i64;
    let failed = |reason: String| WaitOutcome::Failed(format!("Waiter {name} failed: {reason}"));
    loop {
        let response = poll(params)?;
        num_attempts += 1;
        let mut acceptor: Option<&Acceptor> = None;
        let mut matched = false;
        for a in &acceptors {
            acceptor = Some(a);
            if a.matches(&response)? {
                last_matched = true;
                current_state = a.state.clone();
                matched = true;
                break;
            }
        }
        if !matched && is_valid_waiter_error(&response) {
            let error = response.get("Error").unwrap();
            let code = error
                .get("Code")
                .map(pyfmt::py_str)
                .unwrap_or("Unknown".into());
            let message = error
                .get("Message")
                .map(pyfmt::py_str)
                .unwrap_or("Unknown".into());
            return Ok(failed(format!("An error occurred ({code}): {message}")));
        }
        let explanation = || {
            acceptor
                .map(Acceptor::explanation)
                .ok_or_else(|| anyhow::anyhow!("cannot access local variable 'acceptor'"))
        };
        if current_state == "success" {
            return Ok(WaitOutcome::Success);
        }
        if current_state == "failure" {
            return Ok(failed(format!(
                "Waiter encountered a terminal failure state: {}",
                explanation()?
            )));
        }
        if num_attempts as f64 >= max_attempts {
            let reason = if last_matched {
                format!(
                    "Max attempts exceeded. Previously accepted state: {}",
                    explanation()?
                )
            } else {
                "Max attempts exceeded".to_string()
            };
            return Ok(failed(reason));
        }
        tokio::time::sleep(Duration::from_secs_f64(sleep.max(0.0))).await;
    }
}

/// `WaiterStateDocBuilder.build_waiter_state_description`: the help
/// description of a `wait` subcommand.
pub fn waiter_state_description(config: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut description = config
        .get("description")
        .map(py_str_json)
        .unwrap_or_default();
    let operation = config.get("operation").map(py_str_json).unwrap_or_default();
    if description.is_empty() {
        description = "Wait until ".to_string();
        for acceptor in config
            .get("acceptors")
            .and_then(|a| a.as_array())
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if acceptor.get("state").and_then(|s| s.as_str()) == Some("success") {
                let matcher = acceptor
                    .get("matcher")
                    .and_then(|m| m.as_str())
                    .unwrap_or("");
                let expected = acceptor
                    .get("expected")
                    .map(py_str_json)
                    .unwrap_or_default();
                let tail = match matcher {
                    "error" => format!("{expected} is thrown "),
                    "path" => format!("{expected} "),
                    "pathAll" => format!("{expected} for all elements "),
                    "pathAny" => format!("{expected} for any element "),
                    _ => format!("{expected} response is received "),
                };
                if matches!(matcher, "path" | "pathAny" | "pathAll") {
                    let argument = acceptor
                        .get("argument")
                        .map(py_str_json)
                        .unwrap_or_default();
                    description.push_str(&format!("JMESPath query {argument} returns "));
                }
                description.push_str(&tail);
                break;
            }
        }
        description.push_str(&format!(
            "when polling with ``{}``.",
            crate::endpoint::xform_name(&operation, "_").replace('_', "-")
        ));
    }
    let delay = config.get("delay").map(py_str_json).unwrap_or_default();
    let max_attempts = config
        .get("maxAttempts")
        .map(py_str_json)
        .unwrap_or_default();
    description.push_str(&format!(
        " It will poll every {delay} seconds until a successful state has been reached. This will exit with a return code of 255 after {max_attempts} failed checks."
    ));
    description
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Produced by botocore 2.39.3 `Waiter.wait` (with `time.sleep`
    /// recorded) over scripted responses, and by
    /// `WaiterStateDocBuilder.build_waiter_state_description` over every
    /// waiter of the reference models.
    const GOLDEN: &str = r#"{"cases": [["InstanceRunning", {"delay": 15, "operation": "DescribeInstances", "maxAttempts": 3, "acceptors": [{"expected": "running", "matcher": "pathAll", "state": "success", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "shutting-down", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "terminated", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "stopping", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"matcher": "error", "expected": "InvalidInstanceID.NotFound", "state": "retry"}]}, [{"Reservations": [{"Instances": [{"State": {"Name": "pending"}}]}], "ResponseMetadata": {"HTTPStatusCode": 200}}, {"Reservations": [{"Instances": [{"State": {"Name": "running"}}, {"State": {"Name": "running"}}]}], "ResponseMetadata": {"HTTPStatusCode": 200}}], {"outcome": "success", "attempts": 2, "sleeps": [15]}], ["InstanceRunning", {"delay": 15, "operation": "DescribeInstances", "maxAttempts": 3, "acceptors": [{"expected": "running", "matcher": "pathAll", "state": "success", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "shutting-down", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "terminated", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "stopping", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"matcher": "error", "expected": "InvalidInstanceID.NotFound", "state": "retry"}]}, [{"Reservations": [{"Instances": [{"State": {"Name": "pending"}}]}], "ResponseMetadata": {"HTTPStatusCode": 200}}, {"Reservations": [{"Instances": [{"State": {"Name": "running"}}, {"State": {"Name": "terminated"}}]}], "ResponseMetadata": {"HTTPStatusCode": 200}}], {"outcome": "failed", "message": "Waiter InstanceRunning failed: Waiter encountered a terminal failure state: For expression \"Reservations[].Instances[].State.Name\" we matched expected path: \"terminated\" at least once", "attempts": 2, "sleeps": [15]}], ["InstanceRunning", {"delay": 15, "operation": "DescribeInstances", "maxAttempts": 3, "acceptors": [{"expected": "running", "matcher": "pathAll", "state": "success", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "shutting-down", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "terminated", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "stopping", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"matcher": "error", "expected": "InvalidInstanceID.NotFound", "state": "retry"}]}, [{"Reservations": [{"Instances": [{"State": {"Name": "pending"}}]}], "ResponseMetadata": {"HTTPStatusCode": 200}}, {"Reservations": [{"Instances": [{"State": {"Name": "pending"}}]}], "ResponseMetadata": {"HTTPStatusCode": 200}}, {"Reservations": [{"Instances": [{"State": {"Name": "pending"}}]}], "ResponseMetadata": {"HTTPStatusCode": 200}}], {"outcome": "failed", "message": "Waiter InstanceRunning failed: Max attempts exceeded", "attempts": 3, "sleeps": [15, 15]}], ["InstanceRunning", {"delay": 15, "operation": "DescribeInstances", "maxAttempts": 3, "acceptors": [{"expected": "running", "matcher": "pathAll", "state": "success", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "shutting-down", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "terminated", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "stopping", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"matcher": "error", "expected": "InvalidInstanceID.NotFound", "state": "retry"}]}, [{"Reservations": [{"Instances": []}], "ResponseMetadata": {"HTTPStatusCode": 200}}, {"Reservations": [{"Instances": []}], "ResponseMetadata": {"HTTPStatusCode": 200}}, {"Reservations": [{"Instances": []}], "ResponseMetadata": {"HTTPStatusCode": 200}}], {"outcome": "failed", "message": "Waiter InstanceRunning failed: Max attempts exceeded", "attempts": 3, "sleeps": [15, 15]}], ["InstanceRunning", {"delay": 15, "operation": "DescribeInstances", "maxAttempts": 3, "acceptors": [{"expected": "running", "matcher": "pathAll", "state": "success", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "shutting-down", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "terminated", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "stopping", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"matcher": "error", "expected": "InvalidInstanceID.NotFound", "state": "retry"}]}, [{"Error": {"Code": "InvalidInstanceID.NotFound"}, "ResponseMetadata": {"HTTPStatusCode": 400}}, {"Error": {"Code": "InvalidInstanceID.NotFound"}, "ResponseMetadata": {"HTTPStatusCode": 400}}, {"Error": {"Code": "InvalidInstanceID.NotFound"}, "ResponseMetadata": {"HTTPStatusCode": 400}}], {"outcome": "failed", "message": "Waiter InstanceRunning failed: Max attempts exceeded. Previously accepted state: Matched expected service error code: InvalidInstanceID.NotFound", "attempts": 3, "sleeps": [15, 15]}], ["InstanceRunning", {"delay": 15, "operation": "DescribeInstances", "maxAttempts": 3, "acceptors": [{"expected": "running", "matcher": "pathAll", "state": "success", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "shutting-down", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "terminated", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "stopping", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"matcher": "error", "expected": "InvalidInstanceID.NotFound", "state": "retry"}]}, [{"Error": {"Code": "UnauthorizedOperation", "Message": "You are not authorized"}, "ResponseMetadata": {"HTTPStatusCode": 400}}], {"outcome": "failed", "message": "Waiter InstanceRunning failed: An error occurred (UnauthorizedOperation): You are not authorized", "attempts": 1, "sleeps": []}], ["InstanceRunning", {"delay": 15, "operation": "DescribeInstances", "maxAttempts": 3, "acceptors": [{"expected": "running", "matcher": "pathAll", "state": "success", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "shutting-down", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "terminated", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"expected": "stopping", "matcher": "pathAny", "state": "failure", "argument": "Reservations[].Instances[].State.Name"}, {"matcher": "error", "expected": "InvalidInstanceID.NotFound", "state": "retry"}]}, [{"Error": {"Code": "X"}}], {"outcome": "failed", "message": "Waiter InstanceRunning failed: An error occurred (X): Unknown", "attempts": 1, "sleeps": []}], ["TableExists", {"delay": 20, "operation": "DescribeTable", "maxAttempts": 2, "acceptors": [{"expected": "ACTIVE", "matcher": "path", "state": "success", "argument": "Table.TableStatus"}, {"expected": "ResourceNotFoundException", "matcher": "error", "state": "retry"}]}, [{"Error": {"Code": "ResourceNotFoundException"}, "ResponseMetadata": {"HTTPStatusCode": 400}}, {"Table": {"TableStatus": "ACTIVE"}}], {"outcome": "success", "attempts": 2, "sleeps": [20]}], ["TableNotExists", {"delay": 20, "operation": "DescribeTable", "maxAttempts": 2, "acceptors": [{"expected": "ResourceNotFoundException", "matcher": "error", "state": "success"}]}, [{"Table": {"TableStatus": "ACTIVE"}}, {"Error": {"Code": "ResourceNotFoundException"}, "ResponseMetadata": {"HTTPStatusCode": 400}}], {"outcome": "success", "attempts": 2, "sleeps": [20]}], ["BucketExists", {"delay": 5, "operation": "HeadBucket", "maxAttempts": 2, "acceptors": [{"expected": 200, "matcher": "status", "state": "success"}, {"expected": 301, "matcher": "status", "state": "success"}, {"expected": 403, "matcher": "status", "state": "success"}, {"expected": 404, "matcher": "status", "state": "retry"}]}, [{"ResponseMetadata": {"HTTPStatusCode": 404}}, {"ResponseMetadata": {"HTTPStatusCode": 200}}], {"outcome": "success", "attempts": 2, "sleeps": [5]}], ["Custom", {"operation": "Op", "delay": 2, "maxAttempts": 4, "acceptors": [{"state": "retry", "matcher": "path", "argument": "Count", "expected": 1}, {"state": "success", "matcher": "pathAny", "argument": "Items[].Ok", "expected": true}, {"state": "failure", "matcher": "error", "expected": true}]}, [{"Count": 1.0}, {"Count": 2, "Items": [{"Ok": false}]}, {"Count": 2}, {"Count": 2}], {"outcome": "failed", "message": "Waiter Custom failed: Max attempts exceeded. Previously accepted state: Matched expected service error code: True", "attempts": 4, "sleeps": [2, 2, 2]}], ["Custom2", {"operation": "Op", "delay": 1, "maxAttempts": 2, "acceptors": [{"state": "success", "matcher": "error", "expected": false}, {"state": "retry", "matcher": "status", "expected": 500}]}, [{"Error": {"Code": "Throttling"}, "ResponseMetadata": {"HTTPStatusCode": 500}}, {"Error": {"Code": "Throttling"}, "ResponseMetadata": {"HTTPStatusCode": 500}}], {"outcome": "failed", "message": "Waiter Custom2 failed: Max attempts exceeded. Previously accepted state: Matched expected HTTP status code: 500", "attempts": 2, "sleeps": [1]}], ["Custom3", {"operation": "Op", "delay": 0.5, "maxAttempts": 2, "acceptors": [{"state": "failure", "matcher": "pathAll", "argument": "A[].B", "expected": "x"}]}, [{"A": [{"B": "x"}, {"B": "y"}]}, {"A": [{"B": "x"}, {"B": "x"}]}], {"outcome": "failed", "message": "Waiter Custom3 failed: Waiter encountered a terminal failure state: For expression \"A[].B\" all members matched excepted path: \"x\"", "attempts": 2, "sleeps": [0.5]}]], "desc_count": 403, "desc_sha256": "01b71f2dc49de5e9a17343a6e849ef24604c5592802bac6b2f9adcc13adc66cb", "desc_sample": ["Wait until JMESPath query Reservations[].Instances[].State.Name returns running for all elements when polling with ``describe-instances``. It will poll every 15 seconds until a successful state has been reached. This will exit with a return code of 255 after 40 failed checks.", "Wait until 200 response is received when polling with ``head-bucket``. It will poll every 5 seconds until a successful state has been reached. This will exit with a return code of 255 after 20 failed checks.", "Wait until JMESPath query Table.TableStatus returns ACTIVE when polling with ``describe-table``. It will poll every 20 seconds until a successful state has been reached. This will exit with a return code of 255 after 25 failed checks."]}"#;

    #[tokio::test(start_paused = true)]
    async fn waits_match_botocore() {
        let golden: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        for case in golden["cases"].as_array().unwrap() {
            let name = case[0].as_str().unwrap();
            let config = case[1].as_object().unwrap();
            let mut responses: Vec<Value> = case[2]
                .as_array()
                .unwrap()
                .iter()
                .map(Value::from_json)
                .collect();
            responses.reverse();
            let want = &case[3];
            let mut times = Vec::new();
            let start = tokio::time::Instant::now();
            let params = Value::parse_json(r#"{"Foo": "bar"}"#).unwrap();
            let mut poll = |p: &Value| -> Result<Value> {
                assert_eq!(p, &params);
                times.push(start.elapsed().as_secs_f64());
                Ok(responses.pop().expect("too many polls"))
            };
            let outcome = wait(name, config, &params, &mut poll).await.unwrap();
            match (&outcome, want["outcome"].as_str().unwrap()) {
                (WaitOutcome::Success, "success") => {}
                (WaitOutcome::Failed(msg), "failed") => {
                    assert_eq!(msg, want["message"].as_str().unwrap(), "{name}");
                    assert_eq!(outcome.exit_code(), 255);
                    assert_eq!(
                        outcome.stderr().unwrap(),
                        format!("\naws: [ERROR]: {msg}\n")
                    );
                }
                (_, w) => panic!("{name}: want {w}"),
            }
            assert_eq!(
                times.len() as u64,
                want["attempts"].as_u64().unwrap(),
                "{name}"
            );
            let sleeps: Vec<f64> = times.windows(2).map(|w| w[1] - w[0]).collect();
            let want_sleeps: Vec<f64> = want["sleeps"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_f64().unwrap())
                .collect();
            assert_eq!(sleeps, want_sleeps, "{name}");
        }
    }

    #[test]
    fn descriptions_match_awscli() {
        use sha2::Digest;
        let golden: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        let mut names: Vec<&str> = crate::model::service_names().collect();
        names.sort_by_key(|n| format!("{n}/"));
        let mut descs = Vec::new();
        for name in names {
            let model = crate::model::ServiceModel::load(name).unwrap();
            let Some(waiters) = model.waiters() else {
                continue;
            };
            let mut keys: Vec<&String> = waiters.keys().collect();
            keys.sort();
            for k in keys {
                descs.push(waiter_state_description(waiters[k].as_object().unwrap()));
            }
        }
        assert_eq!(descs.len() as u64, golden["desc_count"].as_u64().unwrap());
        let digest = hex::encode(sha2::Sha256::digest(descs.join("\n").as_bytes()));
        assert_eq!(digest, golden["desc_sha256"].as_str().unwrap());
        let ec2 = crate::model::ServiceModel::load("s3").unwrap();
        assert_eq!(
            waiter_state_description(ec2.waiters().unwrap()["BucketExists"].as_object().unwrap()),
            golden["desc_sample"][1].as_str().unwrap()
        );
    }

    #[test]
    fn unknown_matcher_is_a_config_error() {
        let config = serde_json::json!({"operation": "Op", "delay": 1, "maxAttempts": 1,
            "acceptors": [{"state": "success", "matcher": "bogus", "expected": 1}]});
        let mut poll = |_: &Value| -> Result<Value> { Ok(Value::map()) };
        let err = futures_lite_block_on(wait(
            "W",
            config.as_object().unwrap(),
            &Value::map(),
            &mut poll,
        ))
        .err()
        .unwrap();
        assert_eq!(
            err.to_string(),
            "Error processing waiter config: Unknown acceptor: bogus"
        );
    }

    #[test]
    #[ignore = "needs jmes rewrite"]
    fn numeric_projection_matches_across_int_and_float() {
        let config = serde_json::json!({"operation": "Op", "delay": 1, "maxAttempts": 1,
            "acceptors": [{"state": "success", "matcher": "pathAll", "argument": "Items[].N", "expected": 1}]});
        let mut poll = |_: &Value| -> Result<Value> {
            Ok(Value::parse_json(r#"{"Items": [{"N": 1.0}, {"N": 1}]}"#).unwrap())
        };
        let outcome = futures_lite_block_on(wait(
            "W",
            config.as_object().unwrap(),
            &Value::map(),
            &mut poll,
        ))
        .unwrap();
        assert!(matches!(outcome, WaitOutcome::Success));
    }

    fn futures_lite_block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(f)
    }
}
