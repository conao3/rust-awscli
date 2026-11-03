//! `cloudformation deploy` (`deploy.py`, `deployer.py`).

use std::path::Path;

use serde_json::json;

use super::uploader::Uploader;
use super::yamlhelper::yaml_parse;
use super::{CfnError, basic};
use crate::cli::argparse::{Nargs, NsVal};
use crate::commands::services::basic::{ArgDef, Args};
use crate::commands::services::exec::Client;
use crate::driver::{CallResult, Context, client_error};
use crate::error::{CliError, ExitCode};
use crate::value::{Map, Value};
use crate::waiter::{self, WaitOutcome};

fn args() -> Vec<ArgDef> {
    let strings = json!({"type": "array", "items": {"type": "string"}});
    vec![
        ArgDef::new("template-file").required(),
        ArgDef::new("stack-name").required(),
        ArgDef::new("s3-bucket"),
        ArgDef::new("force-upload").flag(),
        ArgDef::new("s3-prefix"),
        ArgDef::new("kms-key-id"),
        ArgDef::new("parameter-overrides")
            .nargs(Nargs::OneOrMore)
            .default(NsVal::List(Vec::new())),
        ArgDef::new("capabilities")
            .schema(json!({
                "type": "array",
                "items": {"type": "string", "enum": ["CAPABILITY_IAM", "CAPABILITY_NAMED_IAM"]},
            }))
            .default(NsVal::List(Vec::new())),
        ArgDef::new("no-execute-changeset")
            .store_false()
            .dest("execute_changeset"),
        ArgDef::new("disable-rollback")
            .flag()
            .dest("disable_rollback")
            .default(NsVal::Bool(false)),
        ArgDef::new("no-disable-rollback")
            .store_false()
            .dest("disable_rollback")
            .default(NsVal::Bool(true)),
        ArgDef::new("role-arn"),
        ArgDef::new("notification-arns").schema(strings.clone()),
        ArgDef::new("fail-on-empty-changeset")
            .flag()
            .dest("fail_on_empty_changeset")
            .default(NsVal::Bool(false)),
        ArgDef::new("no-fail-on-empty-changeset")
            .store_false()
            .dest("fail_on_empty_changeset")
            .default(NsVal::Bool(false)),
        ArgDef::new("tags")
            .schema(strings)
            .default(NsVal::List(Vec::new())),
    ]
}

pub(super) fn run(ctx: &Context, args_in: &[String]) -> Result<ExitCode, CliError> {
    let Some(a) = basic::parse(ctx, &["cloudformation", "deploy"], &args(), args_in)? else {
        return Ok(ExitCode::Success);
    };
    run_main(ctx, &a).map_err(|e| e.render(ctx))
}

fn str_list(v: Option<&Value>) -> Vec<Value> {
    match v {
        Some(Value::List(l)) => l.clone(),
        _ => Vec::new(),
    }
}

/// `parse_key_value_arg`.
fn parse_key_value(values: &[Value], argname: &str) -> Result<Map, CfnError> {
    let mut result = Map::new();
    for data in values {
        let text = match data {
            Value::Str(s) => s.clone(),
            other => crate::pyfmt::py_str(other),
        };
        match text.split_once('=') {
            Some((k, v)) => {
                result.insert(k.to_string(), Value::str(v));
            }
            None => {
                return Err(CfnError::General(format!(
                    "{} value passed to --{argname} must be of format Key=Value",
                    crate::pyfmt::py_repr(&Value::List(vec![Value::Str(text)]))
                )));
            }
        }
    }
    Ok(result)
}

/// `DeployCommand.parse_parameter_overrides`.
fn parse_parameter_overrides(values: &[Value]) -> Result<Map, CfnError> {
    let data = match values.first() {
        Some(Value::Str(first)) => crate::cli::pyjson::loads(first)
            .ok()
            .filter(|v| !matches!(v, Value::Null)),
        _ => None,
    };
    let Some(data) = data else {
        return parse_key_value(values, "parameter-overrides");
    };
    if matches!(data, Value::Int(_) | Value::Float(_) | Value::Bool(_)) {
        return Err(CfnError::General(format!(
            "'{}' object is not iterable",
            crate::cli::shorthand::py_type_name(&data)
        )));
    }
    // CloudFormationLikeParameterOverrideParser.
    if let Value::List(items) = &data {
        for p in items {
            match p {
                Value::Map(m)
                    if m.contains_key("ParameterKey") && m.contains_key("ParameterValue") =>
                {
                    if m.len() > 2 {
                        break;
                    }
                }
                Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null => {
                    return Err(CfnError::General(format!(
                        "argument of type '{}' is not a container or iterable",
                        crate::cli::shorthand::py_type_name(p)
                    )));
                }
                _ => break,
            }
        }
    }
    let cfn_like = match &data {
        Value::List(items) => items.iter().all(|p| match p {
            Value::Map(m) => {
                m.contains_key("ParameterKey") && m.contains_key("ParameterValue") && m.len() <= 2
            }
            _ => false,
        }),
        Value::Map(m) => m.is_empty(),
        Value::Str(s) => s.is_empty(),
        _ => false,
    };
    if cfn_like && let Value::List(items) = &data {
        let mut out = Map::new();
        for p in items {
            let key = p.get("ParameterKey").map(key_text).unwrap_or_default();
            out.insert(key, p.get("ParameterValue").cloned().unwrap_or(Value::Null));
        }
        return Ok(out);
    }
    if cfn_like {
        return Ok(Map::new());
    }
    if let Value::Map(m) = &data
        && let Some(params) = m.get("Parameters")
    {
        return match params {
            Value::Map(p) => Ok(p.clone()),
            _ => Ok(Map::new()),
        };
    }
    let items: Vec<Value> = match &data {
        Value::List(items) => items.clone(),
        Value::Map(m) => m.keys().map(Value::str).collect(),
        _ => Vec::new(),
    };
    if !items.is_empty()
        && items
            .iter()
            .all(|p| matches!(p, Value::Str(s) if s.contains('=')))
    {
        let mut out = Map::new();
        for p in items {
            if let Value::Str(s) = p
                && let Some((k, v)) = s.split_once('=')
            {
                out.insert(k.to_string(), Value::str(v));
            }
        }
        return Ok(out);
    }
    Err(CfnError::Param(
        "JSON passed to --parameter-overrides must be one of the formats: [\"Key1=Value1\",\"Key2=Value2\", ...] , [{\"ParameterKey\": \"Key1\", \"ParameterValue\": \"Value1\"}, ...] , [\"Parameters\": {\"Key1\": \"Value1\", \"Key2\": \"Value2\", ...}]".to_string(),
    ))
}

fn key_text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => crate::pyfmt::py_str(other),
    }
}

/// `DeployCommand.merge_parameters`.
fn merge_parameters(template: &Value, overrides: &Map) -> Vec<Value> {
    let Some(Value::Map(params)) = template.get("Parameters") else {
        return Vec::new();
    };
    params
        .keys()
        .map(|key| {
            let mut obj = Map::new();
            obj.insert("ParameterKey".to_string(), Value::str(key));
            match overrides.get(key) {
                Some(v) => {
                    obj.insert("ParameterValue".to_string(), v.clone());
                }
                None => {
                    obj.insert("UsePreviousValue".to_string(), Value::Bool(true));
                }
            }
            Value::Map(obj)
        })
        .collect()
}

fn os_error(e: &std::io::Error, path: &str) -> String {
    format!(
        "[Errno {}] {}: {}",
        e.raw_os_error().unwrap_or(0),
        match e.kind() {
            std::io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
            std::io::ErrorKind::IsADirectory => "Is a directory".to_string(),
            _ => e.to_string(),
        },
        crate::pyfmt::str_repr(path)
    )
}

fn run_main(ctx: &Context, a: &Args) -> Result<ExitCode, CfnError> {
    let client = Client::new(ctx, "cloudformation")?;
    let template_file = a.str("template-file").unwrap_or_default();
    let template_path = expanduser(&template_file);
    if !Path::new(&template_path).is_file() {
        return Err(CfnError::General(format!(
            "Invalid template path {template_path}"
        )));
    }
    let bytes = std::fs::read(&template_path)
        .map_err(|e| CfnError::General(os_error(&e, &template_path)))?;
    let template_str = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
    let template = yaml_parse(&template_str).map_err(CfnError::General)?;
    let template_size = bytes.len();

    let stack_name = a.str("stack-name").unwrap_or_default();
    let overrides = parse_parameter_overrides(&str_list(a.get("parameter-overrides")))?;
    let tags_dict = parse_key_value(&str_list(a.get("tags")), "tags")?;
    let tags: Vec<Value> = tags_dict
        .into_iter()
        .map(|(k, v)| {
            let mut m = Map::new();
            m.insert("Key".to_string(), Value::Str(k));
            m.insert("Value".to_string(), v);
            Value::Map(m)
        })
        .collect();
    let parameters = merge_parameters(&template, &overrides);
    let bucket = a.str("s3-bucket").filter(|b| !b.is_empty());
    if template_size > 51200 && bucket.is_none() {
        return Err(CfnError::General(
            "Templates with a size greater than 51,200 bytes must be deployed via an S3 Bucket. Please add the --s3-bucket parameter to your command. The local template will be copied to that S3 bucket and then deployed.".to_string(),
        ));
    }
    let uploader = match bucket {
        Some(b) => Some(Uploader::new(
            ctx,
            b,
            a.str("s3-prefix"),
            a.str("kms-key-id"),
            a.bool("force-upload"),
        )?),
        None => None,
    };
    let deployer = Deployer { ctx, client };
    let changeset = deployer.create_changeset(
        &stack_name,
        &template_str,
        parameters,
        a.get("capabilities")
            .cloned()
            .unwrap_or(Value::List(Vec::new())),
        a.get("role-arn").cloned(),
        a.get("notification-arns").cloned(),
        uploader.as_ref(),
        tags,
    )?;
    match deployer.wait_for_changeset(&changeset.0, &stack_name) {
        Ok(()) => {}
        Err(Waited::Empty) => {
            let msg = format!("No changes to deploy. Stack {stack_name} is up to date");
            if a.bool("fail_on_empty_changeset") {
                return Err(CfnError::General(msg));
            }
            basic::print(&format!("\n{msg}\n"));
            return Ok(ExitCode::Success);
        }
        Err(Waited::Error(e)) => return Err(e),
    }
    if a.bool("execute_changeset") {
        deployer.execute_changeset(&changeset.0, &stack_name, a.bool("disable_rollback"))?;
        deployer.wait_for_execute(&stack_name, &changeset.1)?;
        basic::print(&format!(
            "Successfully created/updated stack - {stack_name}\n"
        ));
    } else {
        basic::print(&format!(
            "Changeset created successfully. Run the following command to review changes:\naws cloudformation describe-change-set --change-set-name {}\n",
            changeset.0
        ));
    }
    basic::flush();
    Ok(ExitCode::Success)
}

/// `os.path.expanduser`.
fn expanduser(path: &str) -> String {
    if let Some(rest) = path.strip_prefix('~')
        && (rest.is_empty() || rest.starts_with('/'))
        && let Some(home) = std::env::var_os("HOME")
    {
        let home = home.to_string_lossy();
        let home = home.trim_end_matches('/');
        return format!("{}{rest}", if home.is_empty() { "/" } else { home });
    }
    path.to_string()
}

enum Waited {
    Empty,
    Error(CfnError),
}

struct Deployer<'c> {
    ctx: &'c Context,
    client: Client<'c>,
}

impl Deployer<'_> {
    /// `Deployer.has_stack`.
    fn has_stack(&self, stack_name: &str) -> Result<bool, CfnError> {
        let mut p = Map::new();
        p.insert("StackName".to_string(), Value::str(stack_name));
        match self.client.try_call("DescribeStacks", &Value::Map(p))? {
            CallResult::Ok(resp) => {
                let stacks = match resp.get("Stacks") {
                    Some(Value::List(l)) => l.clone(),
                    Some(_) => Vec::new(),
                    None => return Err(CfnError::General("'Stacks'".to_string())),
                };
                if stacks.len() != 1 {
                    return Ok(false);
                }
                let status = stacks[0]
                    .get("StackStatus")
                    .ok_or_else(|| CfnError::General("'StackStatus'".to_string()))?;
                Ok(status != &Value::str("REVIEW_IN_PROGRESS"))
            }
            CallResult::ServiceError(info) => {
                if super::client_error_text(&info)
                    .contains(&format!("Stack with id {stack_name} does not exist"))
                {
                    Ok(false)
                } else {
                    Err(CfnError::Cli(client_error(self.ctx, &info)))
                }
            }
        }
    }

    /// `Deployer.create_changeset`: the change set id and type.
    #[allow(clippy::too_many_arguments)]
    fn create_changeset(
        &self,
        stack_name: &str,
        template: &str,
        parameters: Vec<Value>,
        capabilities: Value,
        role_arn: Option<Value>,
        notification_arns: Option<Value>,
        uploader: Option<&Uploader<'_>>,
        tags: Vec<Value>,
    ) -> Result<(String, String), CfnError> {
        let now = chrono::Utc::now();
        let description = format!(
            "Created by AWS CLI at {} UTC",
            now.format("%Y-%m-%dT%H:%M:%S%.6f")
        );
        let changeset_name = format!("awscli-cloudformation-package-deploy-{}", now.timestamp());
        let (changeset_type, parameters) = if !self.has_stack(stack_name)? {
            (
                "CREATE",
                parameters
                    .into_iter()
                    .filter(|x| !x.get("UsePreviousValue").is_some_and(Value::is_truthy))
                    .collect::<Vec<_>>(),
            )
        } else {
            let mut p = Map::new();
            p.insert("StackName".to_string(), Value::str(stack_name));
            let summary = self.client.call("GetTemplateSummary", &Value::Map(p))?;
            let existing: Vec<Value> = match summary.get("Parameters") {
                Some(Value::List(l)) => l
                    .iter()
                    .map(|x| x.get("ParameterKey").cloned().unwrap_or(Value::Null))
                    .collect(),
                Some(_) => Vec::new(),
                None => return Err(CfnError::General("'Parameters'".to_string())),
            };
            (
                "UPDATE",
                parameters
                    .into_iter()
                    .filter(|x| {
                        !(x.get("UsePreviousValue").is_some_and(Value::is_truthy)
                            && !existing.contains(x.get("ParameterKey").unwrap_or(&Value::Null)))
                    })
                    .collect(),
            )
        };
        let mut kwargs = Map::new();
        kwargs.insert("ChangeSetName".to_string(), Value::Str(changeset_name));
        kwargs.insert("StackName".to_string(), Value::str(stack_name));
        kwargs.insert("TemplateBody".to_string(), Value::str(template));
        kwargs.insert("ChangeSetType".to_string(), Value::str(changeset_type));
        kwargs.insert("Parameters".to_string(), Value::List(parameters));
        kwargs.insert("Capabilities".to_string(), capabilities);
        kwargs.insert("Description".to_string(), Value::Str(description));
        kwargs.insert("Tags".to_string(), Value::List(tags));
        if let Some(uploader) = uploader {
            let body = kwargs.shift_remove("TemplateBody").unwrap_or(Value::Null);
            let dir = std::env::var("TMPDIR")
                .ok()
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| "/tmp".to_string());
            let file = Path::new(&dir).join(uuid::Uuid::new_v4().simple().to_string());
            std::fs::write(&file, key_text(&body)).map_err(|e| CfnError::General(e.to_string()))?;
            let url = uploader.upload_with_dedup(&file, Some("template"));
            let _ = std::fs::remove_file(&file);
            let url = url?;
            let key = url
                .strip_prefix(&format!("s3://{}/", uploader.bucket))
                .unwrap_or_default()
                .to_string();
            kwargs.insert(
                "TemplateURL".to_string(),
                Value::Str(uploader.to_path_style_s3_url(&key, None)),
            );
        }
        if let Some(r) = role_arn {
            kwargs.insert("RoleARN".to_string(), r);
        }
        if let Some(n) = notification_arns {
            kwargs.insert("NotificationARNs".to_string(), n);
        }
        let resp = self.client.call("CreateChangeSet", &Value::Map(kwargs))?;
        let id = resp
            .get("Id")
            .map(key_text)
            .ok_or_else(|| CfnError::General("'Id'".to_string()))?;
        Ok((id, changeset_type.to_string()))
    }

    /// Run the model waiter `name` with `delay` / `max_attempts`
    /// overrides; the outcome and the last response.
    fn wait(
        &self,
        name: &str,
        params: Value,
        delay: i64,
        max_attempts: Option<i64>,
    ) -> Result<(WaitOutcome, Value), CfnError> {
        let mut config = self
            .client
            .model
            .waiters()
            .and_then(|w| w.get(name))
            .and_then(|w| w.as_object())
            .cloned()
            .ok_or_else(|| CfnError::General(format!("Waiter does not exist: {name}")))?;
        config.insert("delay".to_string(), json!(delay));
        if let Some(m) = max_attempts {
            config.insert("maxAttempts".to_string(), json!(m));
        }
        let operation = config
            .get("operation")
            .and_then(|o| o.as_str())
            .unwrap_or_default()
            .to_string();
        let mut last = Value::Null;
        let mut cli_error = None;
        let mut poll = |p: &Value| -> anyhow::Result<Value> {
            let v = match self.client.try_call(&operation, p) {
                Ok(CallResult::Ok(v)) => v,
                Ok(CallResult::ServiceError(info)) => info.parsed,
                Err(e) => {
                    cli_error = Some(e);
                    anyhow::bail!("call failed");
                }
            };
            last = v.clone();
            Ok(v)
        };
        let outcome = crate::driver::block_on(
            &self.ctx.runtime,
            waiter::wait(name, &config, &params, &mut poll),
        );
        match outcome {
            Ok(o) => Ok((o, last)),
            Err(e) => Err(match cli_error {
                Some(c) => CfnError::Cli(c),
                None => CfnError::General(e.to_string()),
            }),
        }
    }

    /// `Deployer.wait_for_changeset`.
    fn wait_for_changeset(&self, changeset_id: &str, stack_name: &str) -> Result<(), Waited> {
        basic::print("\nWaiting for changeset to be created..\n");
        basic::flush();
        let mut p = Map::new();
        p.insert("ChangeSetName".to_string(), Value::str(changeset_id));
        p.insert("StackName".to_string(), Value::str(stack_name));
        let (outcome, last) = self
            .wait("ChangeSetCreateComplete", Value::Map(p), 5, None)
            .map_err(Waited::Error)?;
        let WaitOutcome::Failed(message) = outcome else {
            return Ok(());
        };
        let status = last
            .get("Status")
            .ok_or_else(|| Waited::Error(CfnError::General("'Status'".to_string())))?;
        let reason = last
            .get("StatusReason")
            .ok_or_else(|| Waited::Error(CfnError::General("'StatusReason'".to_string())))?;
        let reason_text = key_text(reason);
        if (status == &Value::str("FAILED")
            && reason_text.contains("The submitted information didn't contain changes."))
            || reason_text.contains("No updates are to be performed")
        {
            return Err(Waited::Empty);
        }
        Err(Waited::Error(CfnError::General(format!(
            "Failed to create the changeset: {message} Status: {}. Reason: {reason_text}",
            key_text(status)
        ))))
    }

    /// `Deployer.execute_changeset`.
    fn execute_changeset(
        &self,
        changeset_id: &str,
        stack_name: &str,
        disable_rollback: bool,
    ) -> Result<(), CfnError> {
        let mut p = Map::new();
        p.insert("ChangeSetName".to_string(), Value::str(changeset_id));
        p.insert("StackName".to_string(), Value::str(stack_name));
        p.insert("DisableRollback".to_string(), Value::Bool(disable_rollback));
        self.client.call("ExecuteChangeSet", &Value::Map(p))?;
        Ok(())
    }

    /// `Deployer.wait_for_execute`.
    fn wait_for_execute(&self, stack_name: &str, changeset_type: &str) -> Result<(), CfnError> {
        basic::print("Waiting for stack create/update to complete\n");
        basic::flush();
        let name = match changeset_type {
            "CREATE" => "StackCreateComplete",
            _ => "StackUpdateComplete",
        };
        let mut p = Map::new();
        p.insert("StackName".to_string(), Value::str(stack_name));
        let (outcome, _) = self.wait(name, Value::Map(p), 30, Some(120))?;
        if let WaitOutcome::Failed(_) = outcome {
            return Err(CfnError::General(format!(
                "Failed to create/update the stack. Run the following command\nto fetch the list of events leading up to the failure\naws cloudformation describe-stack-events --stack-name {stack_name}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides() {
        let m = parse_parameter_overrides(&[Value::str("A=1"), Value::str("B=x=y")]).unwrap();
        assert_eq!(m["B"], Value::str("x=y"));
        let m = parse_parameter_overrides(&[Value::str(
            r#"[{"ParameterKey": "A", "ParameterValue": "1"}]"#,
        )])
        .unwrap();
        assert_eq!(m["A"], Value::str("1"));
        let m = parse_parameter_overrides(&[Value::str(r#"{"Parameters": {"K": "v"}}"#)]).unwrap();
        assert_eq!(m["K"], Value::str("v"));
        assert!(matches!(
            parse_parameter_overrides(&[Value::str(r#"["x"]"#)]),
            Err(CfnError::Param(_))
        ));
        assert!(matches!(
            parse_parameter_overrides(&[Value::str("A")]),
            Err(CfnError::General(m)) if m == "['A'] value passed to --parameter-overrides must be of format Key=Value"
        ));
    }
}
