//! `dlm create-default-role` (`awscli/customizations/dlm/`).

use serde_json::json;

use super::basic::{self, ArgDef};
use super::exec::{Client, sub_context};
use crate::driver::{CallResult, Context, client_error, display_response};
use crate::error::{CliError, ExitCode};
use crate::output::Response;
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "create-default-role" => create_default_role(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    Value::Map(m)
}

/// `get_policy_arn_suffix` (`awscli/customizations/utils.py`).
pub(crate) fn policy_arn_suffix(region: &str) -> &'static str {
    let r = region.to_lowercase();
    if r.starts_with("cn-") {
        "aws-cn"
    } else if r.starts_with("us-gov") {
        "aws-us-gov"
    } else {
        "aws"
    }
}

/// `IAM.check_if_role_exists` / `check_if_policy_exists`.
fn exists(iam: &Client<'_>, op: &str, params: &Value) -> Result<bool, CliError> {
    match iam.try_call(op, params)? {
        CallResult::Ok(_) => Ok(true),
        CallResult::ServiceError(info) if info.code == "NoSuchEntity" => Ok(false),
        CallResult::ServiceError(info) => Err(client_error(iam.ctx, &info)),
    }
}

/// `CreateDefaultRole._run_main`.
fn create_default_role(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        ArgDef::new("iam-endpoint"),
        ArgDef::new("resource-type")
            .default_str("snapshot")
            .choices(&["snapshot", "image"]),
    ];
    let Some(a) = basic::parse(ctx, &["dlm", "create-default-role"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let region = ctx.globals.region.clone().or_else(|| ctx.session.region());
    let iam_ctx = sub_context(ctx, a.str("iam-endpoint"), region.clone(), None)?;
    let iam = Client::new(&iam_ctx, "iam")?;
    let (role_name, policy_name) = match a.str("resource-type").as_deref() {
        Some("image") => (
            "AWSDataLifecycleManagerDefaultRoleForAMIManagement",
            "AWSDataLifecycleManagerServiceRoleForAMIManagement",
        ),
        _ => (
            "AWSDataLifecycleManagerDefaultRole",
            "AWSDataLifecycleManagerServiceRole",
        ),
    };
    if exists(
        &iam,
        "GetRole",
        &map(vec![("RoleName", Value::str(role_name))]),
    )? {
        return Ok(ExitCode::Success);
    }
    let Some(region) = region else {
        return Err(ctx.structured_error(
            ExitCode::Config,
            "Configuration",
            "You must specify a region. You can also configure your region by running \"aws configure\".",
        ));
    };
    let policy_arn = format!(
        "arn:{}:iam::aws:policy/service-role/{policy_name}",
        policy_arn_suffix(&region)
    );
    if !exists(
        &iam,
        "GetPolicy",
        &map(vec![("PolicyArn", Value::str(&policy_arn))]),
    )? {
        return Ok(ExitCode::Success);
    }
    let assume = json!({
        "Version": "2012-10-17",
        "Statement": [{
            "Sid": "",
            "Effect": "Allow",
            "Principal": {"Service": "dlm.amazonaws.com"},
            "Action": "sts:AssumeRole"
        }]
    });
    let document = crate::pyfmt::json_dumps_default(&crate::value::Value::from_json(&assume));
    let mut create = iam.call(
        "CreateRole",
        &map(vec![
            ("RoleName", Value::str(role_name)),
            ("AssumeRolePolicyDocument", Value::str(document)),
        ]),
    )?;
    iam.call(
        "AttachRolePolicy",
        &map(vec![
            ("PolicyArn", Value::str(&policy_arn)),
            ("RoleName", Value::str(role_name)),
        ]),
    )?;
    let policy = iam.call(
        "GetPolicy",
        &map(vec![("PolicyArn", Value::str(&policy_arn))]),
    )?;
    let version_id = policy
        .get("Policy")
        .and_then(|p| p.get("DefaultVersionId"))
        .cloned()
        .unwrap_or_else(|| Value::str(""));
    let version = iam.call(
        "GetPolicyVersion",
        &map(vec![
            ("PolicyArn", Value::str(&policy_arn)),
            ("VersionId", version_id),
        ]),
    )?;
    let document = version
        .get("PolicyVersion")
        .and_then(|v| v.get("Document"))
        .cloned()
        .unwrap_or_else(Value::map);
    let mut result = Map::new();
    result.insert("RolePolicy".to_string(), document);
    if let Some(m) = create.as_map_mut() {
        m.shift_remove("ResponseMetadata");
        for (k, v) in m.clone() {
            result.insert(k, v);
        }
    }
    display_response(ctx, "create_role", Response::Single(Value::Map(result)))?;
    Ok(ExitCode::Success)
}
