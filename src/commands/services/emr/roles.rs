//! `emr create-default-roles` (`awscli/customizations/emr/createdefaultroles.py`,
//! `configutils.update_roles`).

use indexmap::IndexMap;

use super::super::basic::Args;
use super::super::exec::Client;
use super::config_writer::{self, IniValue};
use super::util::{self, EC2_ROLE_NAME, EMR_ROLE_NAME, general_error};
use crate::driver::{CallResult, Context};
use crate::error::CliError;
use crate::value::{Map, Value};

const ROLES: &[(&str, &str, &[&str])] = &[
    (EC2_ROLE_NAME, "AmazonElasticMapReduceforEC2Role", &["ec2"]),
    (
        EMR_ROLE_NAME,
        "AmazonElasticMapReduceRole",
        &["elasticmapreduce"],
    ),
    (
        "EMR_AutoScaling_DefaultRole",
        "AmazonElasticMapReduceforAutoScalingRole",
        &["elasticmapreduce", "application-autoscaling"],
    ),
];

/// The endpoint of the `emr` client (`client.meta.endpoint_url`).
fn emr_endpoint_url(ctx: &Context, region: Option<&str>) -> String {
    if let Some(url) = &ctx.globals.endpoint_url {
        return url.clone();
    }
    if let Ok(Some(url)) = ctx.session.config().configured_endpoint_url("emr") {
        return url;
    }
    let region = region.unwrap_or("us-east-1");
    let suffix = if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    };
    format!("https://elasticmapreduce.{region}.{suffix}")
}

/// `_get_suffix_and_region_from_endpoint_host`.
fn suffix_and_region(host: &str) -> Option<(String, String)> {
    let first = regex::Regex::new(r"^(https?://)([^.]+).elasticmapreduce.([^/]*)").expect("regex");
    let second = regex::Regex::new(r"^(https?://elasticmapreduce).([^.]+).([^/]*)").expect("regex");
    let caps = first.captures(host).or_else(|| second.captures(host))?;
    Some((caps[3].to_string(), caps[2].to_string()))
}

/// `get_service_principal`.
fn service_principal(ctx: &Context, service: &str, endpoint: &str) -> Result<String, CliError> {
    if service == "ec2" {
        return Ok("ec2.amazonaws.com".to_string());
    }
    let (suffix, region) = suffix_and_region(endpoint).ok_or_else(|| {
        general_error(
            ctx,
            "Could not resolve the service principal from the region or the endpoint.",
        )
    })?;
    if service == "application-autoscaling"
        && !["cn-north-1", "cn-northwest-1"].contains(&region.as_str())
    {
        return Ok("application-autoscaling.amazonaws.com".to_string());
    }
    Ok(format!("{service}.{suffix}"))
}

fn name_param(key: &str, value: &str) -> Value {
    let mut m = Map::new();
    m.insert(key.to_string(), Value::str(value));
    Value::Map(m)
}

/// `check_if_role_exists` / `check_if_instance_profile_exists`: `false` on
/// `NoSuchEntity`.
fn exists(iam: &Client<'_>, operation: &str, params: &Value) -> Result<bool, CliError> {
    match iam.try_call(operation, params)? {
        CallResult::Ok(_) => Ok(true),
        CallResult::ServiceError(info) if info.code == "NoSuchEntity" => Ok(false),
        CallResult::ServiceError(info) => Err(crate::driver::client_error(iam.ctx, &info)),
    }
}

fn call(iam: &Client<'_>, operation: &str, params: Value) -> Result<Value, CliError> {
    let mut v = iam.call(operation, &params)?;
    util::decode_policies(iam, operation, &mut v);
    Ok(v)
}

pub(crate) fn create_default_roles(
    ctx: &Context,
    parsed: &Args,
    region: Option<&str>,
) -> Result<(), CliError> {
    let iam_ctx = util::context_with_endpoint(ctx, parsed.str("iam_endpoint"))?;
    let iam = Client::new(&iam_ctx, "iam")?;
    let emr_endpoint = emr_endpoint_url(ctx, region);
    let mut results: Vec<Value> = Vec::new();
    for (i, (role_name, policy_name, services)) in ROLES.iter().enumerate() {
        if !exists(&iam, "GetRole", &name_param("RoleName", role_name))? {
            let role_arn = format!(
                "arn:{}:iam::aws:policy/service-role/{policy_name}",
                util::policy_arn_suffix(region.unwrap_or_default())
            );
            let principal = if services.len() == 1 {
                Value::str(service_principal(ctx, services[0], &emr_endpoint)?)
            } else {
                let mut l = Vec::new();
                for s in *services {
                    l.push(Value::str(service_principal(ctx, s, &emr_endpoint)?));
                }
                Value::List(l)
            };
            let document = assume_role_policy(principal);
            let mut p = Map::new();
            p.insert("RoleName".to_string(), Value::str(*role_name));
            p.insert(
                "AssumeRolePolicyDocument".to_string(),
                Value::str(util::json_dumps(&document, 2)),
            );
            let created = call(&iam, "CreateRole", Value::Map(p))?;
            let mut p = Map::new();
            p.insert("PolicyArn".to_string(), Value::str(role_arn.clone()));
            p.insert("RoleName".to_string(), Value::str(*role_name));
            call(&iam, "AttachRolePolicy", Value::Map(p))?;
            let policy = call(&iam, "GetPolicy", name_param("PolicyArn", &role_arn))?;
            let mut p = Map::new();
            p.insert("PolicyArn".to_string(), Value::str(role_arn.clone()));
            p.insert(
                "VersionId".to_string(),
                policy
                    .get("Policy")
                    .and_then(|p| p.get("DefaultVersionId"))
                    .cloned()
                    .unwrap_or_default(),
            );
            let version = call(&iam, "GetPolicyVersion", Value::Map(p))?;
            let document = version
                .get("PolicyVersion")
                .and_then(|p| p.get("Document"))
                .cloned()
                .unwrap_or_default();
            if let Some(role) = created.get("Role").filter(|r| !matches!(r, Value::Null)) {
                let mut entry = Map::new();
                entry.insert("Role".to_string(), role.clone());
                entry.insert("RolePolicy".to_string(), document);
                results.push(Value::Map(entry));
            }
        }
        if i == 0 {
            let profile = name_param("InstanceProfileName", EC2_ROLE_NAME);
            if !exists(&iam, "GetInstanceProfile", &profile)? {
                call(&iam, "CreateInstanceProfile", profile)?;
                let mut p = Map::new();
                p.insert("InstanceProfileName".to_string(), Value::str(EC2_ROLE_NAME));
                p.insert("RoleName".to_string(), Value::str(EC2_ROLE_NAME));
                call(&iam, "AddRoleToInstanceProfile", Value::Map(p))?;
            }
        }
    }
    update_roles(ctx)?;
    util::display(ctx, "create_role", Value::List(results))
}

/// `assume_role_policy`.
fn assume_role_policy(principal: Value) -> Value {
    let mut p = Map::new();
    p.insert("Service".to_string(), principal);
    let mut st = Map::new();
    st.insert("Sid".to_string(), Value::str(""));
    st.insert("Effect".to_string(), Value::str("Allow"));
    st.insert("Principal".to_string(), Value::Map(p));
    st.insert("Action".to_string(), Value::str("sts:AssumeRole"));
    let mut doc = Map::new();
    doc.insert("Version".to_string(), Value::str("2008-10-17"));
    doc.insert("Statement".to_string(), Value::List(vec![Value::Map(st)]));
    Value::Map(doc)
}

/// `configutils.update_roles`.
fn update_roles(ctx: &Context) -> Result<(), CliError> {
    let configs = util::emr_configs(ctx);
    if configs.contains_key("instance_profile") || configs.contains_key("service_role") {
        return Ok(());
    }
    let section = match ctx.session.config().profile() {
        Some(p) => format!("profile {p}"),
        None => "default".to_string(),
    };
    let filename = ctx.session.config().config_file_path();
    for (key, value) in [
        ("service_role", EMR_ROLE_NAME),
        ("instance_profile", EC2_ROLE_NAME),
    ] {
        let mut sub = IndexMap::new();
        sub.insert(key.to_string(), value.to_string());
        let mut v = IndexMap::new();
        v.insert("emr".to_string(), IniValue::Nested(sub));
        config_writer::update_config(&section, v, &filename)
            .map_err(|e| general_error(ctx, &e.to_string()))?;
    }
    Ok(())
}
