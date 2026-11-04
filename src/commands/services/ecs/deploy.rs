//! `ecs deploy` (`awscli/customizations/ecs/deploy.py`, `filehelpers.py`,
//! `exceptions.py`).

use sha2::Digest as _;

use crate::commands::services::basic::{self, ArgDef, Args};
use crate::commands::services::exec::Client;
use crate::driver::{CallResult, Context};
use crate::error::{CliError, ExitCode};
use crate::model::ServiceModel;
use crate::pyfmt::{json_dumps_default, py_str};
use crate::value::{Map, Value};

const TIMEOUT_BUFFER_MIN: i64 = 10;
const DEFAULT_DELAY_SEC: i64 = 15;
const MAX_WAIT_MIN: i64 = 360;
const MAX_CHAR_LENGTH: usize = 46;

fn ecs_error(ctx: &Context, message: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("{message}"))
}

fn key_error(ctx: &Context, key: &str) -> CliError {
    ecs_error(ctx, &format!("'{key}'"))
}

fn get<'v>(ctx: &Context, v: &'v Value, key: &str) -> Result<&'v Value, CliError> {
    v.get(key).ok_or_else(|| key_error(ctx, key))
}

/// A call whose `ClientError` becomes `ServiceClientError(action, error)`.
fn call(
    client: &Client<'_>,
    operation: &str,
    params: &Value,
    action: &str,
) -> Result<Value, CliError> {
    match client.try_call(operation, params)? {
        CallResult::Ok(v) => Ok(v),
        CallResult::ServiceError(info) => Err(ecs_error(
            client.ctx,
            &format!("Failed to {action}:\n{}", info.exception_message()),
        )),
    }
}

/// `_get_file_contents`.
fn file_contents(ctx: &Context, path: &str) -> Result<String, CliError> {
    let full = crate::cli::paramfile::expanduser(&crate::cli::paramfile::expandvars(path));
    let error = match std::fs::read(&full) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(s) => return Ok(s),
            Err(e) => format!(
                "'utf-8' codec can't decode byte 0x{:02x} in position {}: invalid start byte",
                e.as_bytes()[e.utf8_error().valid_up_to()],
                e.utf8_error().valid_up_to()
            ),
        },
        Err(e) => crate::cli::paramfile::os_error(&e, &full),
    };
    Err(ecs_error(
        ctx,
        &format!("Unable to load file at {path}: {error}"),
    ))
}

/// `filehelpers.parse_appspec`: JSON, else YAML.
fn parse_appspec(text: &str) -> Value {
    if let Ok(v) = crate::cli::pyjson::loads(text) {
        return v;
    }
    serde_yaml::from_str::<serde_yaml::Value>(text)
        .ok()
        .map(|y| yaml_value(&y))
        .unwrap_or(Value::Null)
}

fn yaml_value(v: &serde_yaml::Value) -> Value {
    match v {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(b) => Value::Bool(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_yaml::Value::String(s) => Value::Str(s.clone()),
        serde_yaml::Value::Sequence(s) => Value::List(s.iter().map(yaml_value).collect()),
        serde_yaml::Value::Mapping(m) => Value::Map(
            m.iter()
                .map(|(k, v)| {
                    let key = match k {
                        serde_yaml::Value::String(s) => s.clone(),
                        other => py_str(&yaml_value(other)),
                    };
                    (key, yaml_value(v))
                })
                .collect(),
        ),
        serde_yaml::Value::Tagged(t) => yaml_value(&t.value),
    }
}

/// `filehelpers.find_required_key`.
fn find_required_key(
    ctx: &Context,
    resource: &str,
    obj: &Value,
    key: &str,
) -> Result<String, CliError> {
    let missing = || {
        ecs_error(
            ctx,
            &format!("Resource '{resource}' must include property '{key}'"),
        )
    };
    let Value::Map(m) = obj else {
        return Err(missing());
    };
    m.keys()
        .find(|k| k.to_lowercase() == key.to_lowercase())
        .cloned()
        .ok_or_else(missing)
}

/// `CodeDeployer.update_task_def_arn`.
fn update_task_def_arn(ctx: &Context, appspec: &mut Value, arn: &str) -> Result<(), CliError> {
    let resources_key = find_required_key(ctx, "codedeploy-appspec", appspec, "resources")?;
    let resources = appspec
        .as_map_mut()
        .and_then(|m| m.get_mut(&resources_key))
        .ok_or_else(|| key_error(ctx, &resources_key))?;
    let Value::List(items) = resources else {
        return Ok(());
    };
    for resource in items.iter_mut() {
        let Value::Map(resource) = resource else {
            continue;
        };
        for (name, content) in resource.iter_mut() {
            let props_key = find_required_key(ctx, name, content, "properties")?;
            let props = content
                .as_map_mut()
                .and_then(|m| m.get_mut(&props_key))
                .expect("found key");
            let task_key = find_required_key(ctx, &props_key, props, "taskDefinition")?;
            if let Some(m) = props.as_map_mut() {
                m.insert(task_key, Value::str(arn));
            }
        }
    }
    Ok(())
}

/// `_get_ecs_suffix`.
fn ecs_suffix(service: &str, cluster: &str) -> String {
    let take = |s: &str| s.chars().take(MAX_CHAR_LENGTH).collect::<String>();
    format!("{}-{}", take(cluster), take(service))
}

pub(super) fn deploy(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        ArgDef::new("service").required(),
        ArgDef::new("task-definition").required(),
        ArgDef::new("codedeploy-appspec").required(),
        ArgDef::new("cluster"),
        ArgDef::new("codedeploy-application"),
        ArgDef::new("codedeploy-deployment-group"),
    ];
    let Some(a) = basic::parse(ctx, &["ecs", "deploy"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    run_main(ctx, &a)
}

fn run_main(ctx: &Context, a: &Args) -> Result<ExitCode, CliError> {
    let task_def_text = file_contents(ctx, &a.str("task-definition").unwrap_or_default())?;
    let task_def = crate::cli::pyjson::loads(&task_def_text).map_err(|e| ecs_error(ctx, &e))?;
    let appspec_text = file_contents(ctx, &a.str("codedeploy-appspec").unwrap_or_default())?;
    let mut appspec = parse_appspec(&appspec_text);
    let ecs = Client::new(ctx, "ecs")?;
    let service_arg = a.str("service").unwrap_or_default();
    let cluster = a.str("cluster").unwrap_or_else(|| "default".to_string());
    let mut p = Map::new();
    p.insert("cluster".to_string(), Value::str(&cluster));
    p.insert(
        "services".to_string(),
        Value::List(vec![Value::str(&service_arg)]),
    );
    let services = call(
        &ecs,
        "DescribeServices",
        &Value::Map(p),
        "describe ECS service",
    )?;
    let list = get(ctx, &services, "services")?
        .as_list()
        .cloned()
        .unwrap_or_default();
    let Some(details) = list.first() else {
        return Err(ecs_error(
            ctx,
            &format!("Service '{service_arg}' not found in cluster '{cluster}'"),
        ));
    };
    let cluster_arn = py_str(get(ctx, details, "clusterArn")?);
    let cluster_name = cluster_arn
        .split('/')
        .nth(1)
        .ok_or_else(|| ecs_error(ctx, "list index out of range"))?
        .to_string();
    let service_arn = py_str(get(ctx, details, "serviceArn")?);
    let service_name = py_str(get(ctx, details, "serviceName")?);
    let app_name = a
        .str("codedeploy-application")
        .unwrap_or_else(|| format!("AppECS-{}", ecs_suffix(&service_name, &cluster_name)));
    let group_name = a
        .str("codedeploy-deployment-group")
        .unwrap_or_else(|| format!("DgpECS-{}", ecs_suffix(&service_name, &cluster_name)));

    let cd = Client::new(ctx, "codedeploy")?;
    let mut p = Map::new();
    p.insert("applicationName".to_string(), Value::str(&app_name));
    let app = call(
        &cd,
        "GetApplication",
        &Value::Map(p.clone()),
        "describe Code Deploy application",
    )?;
    p.insert("deploymentGroupName".to_string(), Value::str(&group_name));
    let group = call(
        &cd,
        "GetDeploymentGroup",
        &Value::Map(p),
        "describe Code Deploy deployment group",
    )?;
    if py_str(get(ctx, get(ctx, &app, "application")?, "computePlatform")?) != "ECS" {
        return Err(ecs_error(
            ctx,
            &format!("Application '{app_name}' must support 'ECS' compute platform"),
        ));
    }
    let info = get(ctx, &group, "deploymentGroupInfo")?;
    if py_str(get(ctx, info, "computePlatform")?) != "ECS" {
        return Err(ecs_error(
            ctx,
            &format!("Deployment Group '{group_name}' must support 'ECS' compute platform"),
        ));
    }
    for target in get(ctx, info, "ecsServices")?
        .as_list()
        .into_iter()
        .flatten()
    {
        let s = py_str(get(ctx, target, "serviceName")?);
        if s != service_name && s != service_arn {
            return Err(ecs_error(
                ctx,
                &format!(
                    "deployment group '{group_name}' does not target ECS service '{service_name}'"
                ),
            ));
        }
        let c = py_str(get(ctx, target, "clusterName")?);
        if c != cluster_name && c != cluster_arn {
            return Err(ecs_error(
                ctx,
                &format!(
                    "deployment group '{group_name}' does not target ECS cluster '{cluster_name}'"
                ),
            ));
        }
    }
    let blue_green = get(ctx, info, "blueGreenDeploymentConfiguration")?;
    let ready = get(
        ctx,
        get(ctx, blue_green, "deploymentReadyOption")?,
        "waitTimeInMinutes",
    )?
    .as_i64()
    .unwrap_or_default();
    let terminate = get(
        ctx,
        get(ctx, blue_green, "terminateBlueInstancesOnDeploymentSuccess")?,
        "terminationWaitTimeInMinutes",
    )?
    .as_i64()
    .unwrap_or_default();
    let wait_time = ready + terminate + TIMEOUT_BUFFER_MIN;

    let registered = call(
        &ecs,
        "RegisterTaskDefinition",
        &task_def,
        "register ECS task definition",
    )?;
    let task_def_arn = py_str(get(
        ctx,
        get(ctx, &registered, "taskDefinition")?,
        "taskDefinitionArn",
    )?);
    basic::print(&format!(
        "Successfully registered new ECS task definition {task_def_arn}\n"
    ));
    basic::flush();

    update_task_def_arn(ctx, &mut appspec, &task_def_arn)?;
    let content = json_dumps_default(&appspec);
    let sha = hex::encode(sha2::Sha256::digest(content.as_bytes()));
    let request = Value::from_json(&serde_json::json!({
        "applicationName": app_name,
        "deploymentGroupName": group_name,
        "revision": {
            "revisionType": "AppSpecContent",
            "appSpecContent": {"content": content, "sha256": sha},
        },
    }));
    let created = call(&cd, "CreateDeployment", &request, "create deployment")?;
    let deployment_id = py_str(get(ctx, &created, "deploymentId")?);
    basic::print(&format!(
        "Successfully created deployment {deployment_id}\n"
    ));

    let wait_min = wait_time.clamp(30, MAX_WAIT_MIN);
    basic::print(&format!(
        "Waiting for {deployment_id} to succeed (will wait up to {wait_min} minutes)...\n"
    ));
    basic::flush();
    wait_for_deployment(ctx, &cd.model, &deployment_id, wait_min)?;
    basic::print(&format!(
        "Successfully deployed {task_def_arn} to service '{service_name}'\n"
    ));
    basic::flush();
    Ok(ExitCode::Success)
}

/// `CodeDeployer.wait_for_deploy_success`.
fn wait_for_deployment(
    ctx: &Context,
    model: &ServiceModel,
    deployment_id: &str,
    wait_min: i64,
) -> Result<(), CliError> {
    let mut config = model
        .waiters()
        .and_then(|w| w.get("DeploymentSuccessful"))
        .and_then(|w| w.as_object())
        .map(crate::bjson::Obj::to_map)
        .ok_or_else(|| ecs_error(ctx, "Waiter does not exist: deployment_successful"))?;
    config.insert("delay".to_string(), serde_json::json!(DEFAULT_DELAY_SEC));
    config.insert(
        "maxAttempts".to_string(),
        serde_json::json!((wait_min * 60) as f64 / DEFAULT_DELAY_SEC as f64),
    );
    let caller = crate::driver::Caller::new(ctx, model, "GetDeployment")
        .map_err(|e| ctx.general_error(&e))?;
    let mut poll = |params: &Value| -> anyhow::Result<Value> {
        Ok(match caller.call(params)? {
            CallResult::Ok(v) => v,
            CallResult::ServiceError(info) => info.parsed,
        })
    };
    let mut params = Map::new();
    params.insert("deploymentId".to_string(), Value::str(deployment_id));
    let outcome = crate::driver::block_on(
        &ctx.runtime,
        crate::waiter::wait(
            "DeploymentSuccessful",
            &config,
            &Value::Map(params),
            &mut poll,
        ),
    )
    .map_err(|e| ctx.general_error(&e))?;
    match outcome {
        crate::waiter::WaitOutcome::Success => Ok(()),
        crate::waiter::WaitOutcome::Failed(msg) => Err(ecs_error(ctx, msg.trim())),
    }
}
