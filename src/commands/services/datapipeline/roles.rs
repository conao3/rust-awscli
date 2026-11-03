//! `datapipeline create-default-roles`
//! (`datapipeline/createdefaultroles.py`).

use crate::commands::services::basic;
use crate::commands::services::exec::Client;
use crate::driver::{CallResult, Context, client_error, display_response};
use crate::error::{CliError, ExitCode};
use crate::output::Response;
use crate::value::{Map, Value};

const SERVICE_ROLE_NAME: &str = "DataPipelineDefaultRole";
const RESOURCE_ROLE_NAME: &str = "DataPipelineDefaultResourceRole";
const SERVICE_ROLE_ARN: &str = "arn:aws:iam::aws:policy/service-role/AWSDataPipelineRole";
const RESOURCE_ROLE_ARN: &str =
    "arn:aws:iam::aws:policy/service-role/AmazonEC2RoleforDataPipelineRole";

/// `dict_to_string(DATAPIPELINE_DEFAULT_SERVICE_ROLE_ASSUME_POLICY)`.
const SERVICE_ROLE_ASSUME_POLICY: &str = r#"{
  "Version": "2008-10-17",
  "Statement": [
    {
      "Sid": "",
      "Effect": "Allow",
      "Principal": {
        "Service": [
          "datapipeline.amazonaws.com",
          "elasticmapreduce.amazonaws.com"
        ]
      },
      "Action": "sts:AssumeRole"
    }
  ]
}"#;

/// `dict_to_string(DATAPIPELINE_DEFAULT_RESOURCE_ROLE_ASSUME_POLICY)`.
const RESOURCE_ROLE_ASSUME_POLICY: &str = r#"{
  "Version": "2008-10-17",
  "Statement": [
    {
      "Sid": "",
      "Effect": "Allow",
      "Principal": {
        "Service": "ec2.amazonaws.com"
      },
      "Action": "sts:AssumeRole"
    }
  ]
}"#;

const DEPRECATION_WARNING: &str = "awscli/customizations/datapipeline/createdefaultroles.py:83: UserWarning: \nSupport for this command has been deprecated and may fail to create these roles\nif they do not already exist. For more information on managing these policies\nmanually see the following documentation:\n\nhttps://docs.aws.amazon.com/datapipeline/latest/DeveloperGuide/dp-iam-roles.html\n\n";

fn params(pairs: &[(&str, &str)]) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), Value::str(*v));
    }
    Value::Map(m)
}

/// `_check_if_role_exists` / `_check_if_instance_profile_exists`.
fn exists(client: &Client<'_>, operation: &str, p: Value) -> Result<bool, CliError> {
    match client.try_call(operation, &p)? {
        CallResult::Ok(_) => Ok(true),
        CallResult::ServiceError(info) if info.code == "NoSuchEntity" => Ok(false),
        CallResult::ServiceError(info) => Err(client_error(client.ctx, &info)),
    }
}

/// `botocore.handlers.decode_quoted_jsondoc` (the IAM `json_decode_policies`
/// handler on `GetPolicyVersion`).
fn decode_quoted_jsondoc(value: &Value) -> Value {
    let Value::Str(s) = value else {
        return value.clone();
    };
    let unquoted = percent_encoding::percent_decode_str(s).decode_utf8_lossy();
    crate::cli::pyjson::loads(&unquoted).unwrap_or_else(|_| value.clone())
}

fn key_error(ctx: &Context, key: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("'{key}'"))
}

/// `_create_role`: the `CreateRole` response and the policy document of a
/// role that was created.
fn create_role(
    client: &Client<'_>,
    name: &str,
    arn: &str,
    policy: &str,
) -> Result<Option<(Value, Value)>, CliError> {
    if exists(client, "GetRole", params(&[("RoleName", name)]))? {
        return Ok(None);
    }
    let created = client.call(
        "CreateRole",
        &params(&[("RoleName", name), ("AssumeRolePolicyDocument", policy)]),
    )?;
    client.call(
        "AttachRolePolicy",
        &params(&[("PolicyArn", arn), ("RoleName", name)]),
    )?;
    let details = client.call("GetPolicy", &params(&[("PolicyArn", arn)]))?;
    let version = details
        .get("Policy")
        .ok_or_else(|| key_error(client.ctx, "Policy"))?
        .get("DefaultVersionId")
        .and_then(Value::as_str)
        .ok_or_else(|| key_error(client.ctx, "DefaultVersionId"))?
        .to_string();
    let version = client.call(
        "GetPolicyVersion",
        &params(&[("PolicyArn", arn), ("VersionId", &version)]),
    )?;
    let document = version
        .get("PolicyVersion")
        .ok_or_else(|| key_error(client.ctx, "PolicyVersion"))?
        .get("Document")
        .map(decode_quoted_jsondoc)
        .ok_or_else(|| key_error(client.ctx, "Document"))?;
    Ok(Some((created, document)))
}

pub(super) fn create_default_roles(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    if basic::parse(ctx, &["datapipeline", "create-default-roles"], &[], args)?.is_none() {
        return Ok(ExitCode::Success);
    }
    let client = Client::new(ctx, "iam")?;
    basic::eprint(DEPRECATION_WARNING);
    let service = create_role(
        &client,
        SERVICE_ROLE_NAME,
        SERVICE_ROLE_ARN,
        SERVICE_ROLE_ASSUME_POLICY,
    )?;
    let resource = create_role(
        &client,
        RESOURCE_ROLE_NAME,
        RESOURCE_ROLE_ARN,
        RESOURCE_ROLE_ASSUME_POLICY,
    )?;
    let profile = params(&[("InstanceProfileName", RESOURCE_ROLE_NAME)]);
    if !exists(&client, "GetInstanceProfile", profile.clone())? {
        client.call("CreateInstanceProfile", &profile)?;
        client.call(
            "AddRoleToInstanceProfile",
            &params(&[
                ("InstanceProfileName", RESOURCE_ROLE_NAME),
                ("RoleName", RESOURCE_ROLE_NAME),
            ]),
        )?;
    }
    let mut result = Vec::new();
    for (response, policy) in [service, resource].into_iter().flatten() {
        let role = response
            .get("Role")
            .cloned()
            .ok_or_else(|| key_error(ctx, "Role"))?;
        if !matches!(role, Value::Null) {
            let mut m = Map::new();
            m.insert("Role".to_string(), role);
            m.insert("RolePolicy".to_string(), policy);
            result.push(Value::Map(m));
        }
    }
    display_response(ctx, "create_role", Response::Single(Value::List(result)))?;
    Ok(ExitCode::Success)
}
