//! Helpers shared by the `emr` commands (`awscli/customizations/emr/`
//! `emrutils.py`, `constants.py`, `exceptions.py`, `command.py`,
//! `config.py`, `configutils.py`).

use std::sync::OnceLock;

use serde_json::Value as Json;

use super::super::basic::{self, ArgDef, Args};
use super::super::exec::Client;
use crate::cli::argparse::{Nargs, NsVal};
use crate::driver::{Context, display_response};
use crate::error::CliError;
use crate::output::Response;
use crate::value::{Map, Value};

pub(crate) const EC2_ROLE_NAME: &str = "EMR_EC2_DefaultRole";
pub(crate) const EMR_ROLE_NAME: &str = "EMR_DefaultRole";
pub(crate) const DEFAULT_FAILURE_ACTION: &str = "CONTINUE";
pub(crate) const CANCEL_AND_WAIT: &str = "CANCEL_AND_WAIT";
pub(crate) const TERMINATE_CLUSTER: &str = "TERMINATE_CLUSTER";
pub(crate) const SCRIPT_RUNNER_PATH: &str = "/libs/script-runner/script-runner.jar";
pub(crate) const COMMAND_RUNNER: &str = "command-runner.jar";
pub(crate) const HBASE_JAR_PATH: &str = "/home/hadoop/lib/hbase.jar";
pub(crate) const HBASE_MAIN: &str = "emr.hbase.backup.Main";
pub(crate) const LATEST: &str = "latest";
pub(crate) const APPLICATIONS: &[&str] = &[
    "HIVE", "PIG", "HBASE", "GANGLIA", "IMPALA", "SPARK", "MAPR", "MAPR_M3", "MAPR_M5", "MAPR_M7",
];

/// The `ARG_TABLE`s of the `emr` and `emr-containers` commands, dumped from
/// the reference by `gen_tables.py` (help texts dropped).
fn tables() -> &'static serde_json::Map<String, Json> {
    static T: OnceLock<serde_json::Map<String, Json>> = OnceLock::new();
    T.get_or_init(|| serde_json::from_str(include_str!("tables.json")).expect("emr tables"))
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// `ARG_TABLE` of `command` as [`ArgDef`]s.
pub(crate) fn arg_defs(command: &str) -> Vec<ArgDef> {
    let Some(Json::Array(entries)) = tables().get(command) else {
        return Vec::new();
    };
    entries
        .iter()
        .map(|e| {
            let mut d = ArgDef::new(leak(e["name"].as_str().unwrap_or_default()));
            if e.get("required").and_then(Json::as_bool) == Some(true) {
                d = d.required();
            }
            match e.get("action").and_then(Json::as_str) {
                Some("store_true") => d = d.flag(),
                Some("store_false") => d = d.store_false(),
                _ => {}
            }
            if e.get("nargs").and_then(Json::as_str) == Some("+") {
                d = d.nargs(Nargs::OneOrMore);
            }
            match e.get("default") {
                Some(Json::String(s)) => d = d.default_str(s),
                Some(Json::Bool(b)) => d = d.default(NsVal::Bool(*b)),
                _ => {}
            }
            if let Some(Json::Array(c)) = e.get("choices") {
                let c: Vec<&str> = c.iter().filter_map(Json::as_str).collect();
                d = d.choices(&c);
            }
            if let Some(t) = e.get("cli_type_name").and_then(Json::as_str) {
                d = d.type_name(leak(t));
            }
            if let Some(s) = e.get("schema") {
                d = d.schema(s.clone());
            }
            d
        })
        .collect()
}

/// `EmrError` subclasses that are also `ParamValidationError`s (exit 252).
pub(crate) fn param_error(ctx: &Context, message: &str) -> CliError {
    basic::param_validation(ctx, message)
}

/// Any other exception (exit 255).
pub(crate) fn general_error(ctx: &Context, message: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("{message}"))
}

/// `MissingParametersError`.
pub(crate) fn missing_parameters(ctx: &Context, object_name: &str, missing: &str) -> CliError {
    param_error(
        ctx,
        &format!("The following required parameters are missing for {object_name}: {missing}."),
    )
}

/// `MutualExclusiveOptionError`.
pub(crate) fn mutual_exclusive(
    ctx: &Context,
    option1: &str,
    option2: &str,
    message: &str,
) -> CliError {
    param_error(
        ctx,
        &format!("You cannot specify both {option1} and {option2} options together.{message}"),
    )
}

/// `configutils.get_configs`: the `emr` nested section of the profile.
pub(crate) fn emr_configs(ctx: &Context) -> Map {
    let mut out = Map::new();
    if let Ok(section) = ctx.session.config().scoped_config()
        && let Some(nested) = section.get("emr").and_then(|v| v.as_nested())
    {
        for (k, v) in nested {
            out.insert(k.clone(), Value::str(v.clone()));
        }
    }
    out
}

/// `override_args_required_option`: arguments set in the configuration are
/// not required.
pub(crate) fn override_required(ctx: &Context, defs: &mut [ArgDef], args: &[String]) {
    if args.len() == 1 && args[0] == "help" {
        return;
    }
    let configs = emr_configs(ctx);
    for d in defs.iter_mut() {
        if configs.contains_key(&d.name.replace('-', "_")) {
            d.required = false;
        }
    }
}

/// `emrutils.get_region`.
pub(crate) fn region(ctx: &Context) -> Option<String> {
    ctx.globals.region.clone().or_else(|| ctx.session.region())
}

/// `emrutils.build_s3_link`.
pub(crate) fn s3_link(relative_path: &str, region: Option<&str>) -> String {
    format!(
        "s3://{}.elasticmapreduce{relative_path}",
        region.unwrap_or("us-east-1")
    )
}

/// `emrutils.get_script_runner`.
pub(crate) fn script_runner(region: Option<&str>) -> String {
    s3_link(SCRIPT_RUNNER_PATH, region)
}

/// `emrutils.parse_tags`.
pub(crate) fn parse_tags(raw: &[String]) -> Value {
    Value::List(
        raw.iter()
            .map(|t| {
                let (k, v) = split_to_key_value(t);
                let mut m = Map::new();
                m.insert("Key".to_string(), Value::str(k));
                m.insert("Value".to_string(), Value::str(v));
                Value::Map(m)
            })
            .collect(),
    )
}

/// `emrutils.split_to_key_value`.
pub(crate) fn split_to_key_value(s: &str) -> (String, String) {
    match s.split_once('=') {
        Some((k, v)) => (k.to_string(), v.to_string()),
        None => (s.to_string(), String::new()),
    }
}

/// `emrutils.parse_key_value_string`.
pub(crate) fn parse_key_value_string(s: Option<&Value>) -> Option<Value> {
    let s = s.and_then(Value::as_str)?;
    Some(Value::List(
        s.split(',')
            .map(|kv| {
                let (k, v) = split_to_key_value(kv);
                let mut m = Map::new();
                m.insert("Key".to_string(), Value::str(k));
                m.insert("Value".to_string(), Value::str(v));
                Value::Map(m)
            })
            .collect(),
    ))
}

/// `emrutils.apply_dict`: set `key` when `value` is truthy.
pub(crate) fn apply(params: &mut Map, key: &str, value: Option<Value>) {
    if let Some(v) = value.filter(Value::is_truthy) {
        params.insert(key.to_string(), v);
    }
}

/// `emrutils.apply_params`.
pub(crate) fn apply_params(src: &Map, src_key: &str, dest: &mut Map, dest_key: &str) {
    if let Some(v) = src.get(src_key).filter(|v| v.is_truthy()) {
        dest.insert(dest_key.to_string(), v.clone());
    }
}

/// `emrutils.apply_boolean_options`.
pub(crate) fn apply_boolean_options(
    ctx: &Context,
    true_option: bool,
    true_name: &str,
    false_option: bool,
    false_name: &str,
) -> Result<bool, CliError> {
    if true_option && false_option {
        return Err(param_error(
            ctx,
            &format!("cannot use both {true_name} and {false_name} options together."),
        ));
    }
    Ok(true_option)
}

/// The arguments of `emrutils.build_step`.
pub(crate) struct Step<'a> {
    pub jar: Option<Value>,
    pub name: &'a str,
    pub action_on_failure: &'a str,
    pub args: Option<Value>,
    pub main_class: Option<Value>,
    pub properties: Option<Value>,
    pub log_uri: Option<Value>,
    pub encryption_key_arn: Option<Value>,
}

impl<'a> Step<'a> {
    pub(crate) fn new(
        jar: &str,
        name: &'a str,
        action_on_failure: &'a str,
        args: Vec<String>,
    ) -> Step<'a> {
        Step {
            jar: Some(Value::str(jar)),
            name,
            action_on_failure,
            args: Some(str_list(&args)),
            main_class: None,
            properties: None,
            log_uri: None,
            encryption_key_arn: None,
        }
    }
}

pub(crate) fn str_list(items: &[String]) -> Value {
    Value::List(items.iter().map(|s| Value::str(s.clone())).collect())
}

/// `emrutils.build_step`.
pub(crate) fn build_step(ctx: &Context, s: Step<'_>) -> Result<Value, CliError> {
    check_required_field(ctx, "HadoopJarStep", "Jar", s.jar.as_ref())?;
    let mut step = Map::new();
    apply(&mut step, "Name", Some(Value::str(s.name)));
    apply(
        &mut step,
        "ActionOnFailure",
        Some(Value::str(s.action_on_failure)),
    );
    let mut jar_config = Map::new();
    jar_config.insert("Jar".to_string(), s.jar.unwrap_or_default());
    apply(&mut jar_config, "Args", s.args);
    apply(&mut jar_config, "MainClass", s.main_class);
    apply(&mut jar_config, "Properties", s.properties);
    step.insert("HadoopJarStep".to_string(), Value::Map(jar_config));
    let mut s3 = Map::new();
    apply(&mut s3, "LogUri", s.log_uri);
    apply(&mut s3, "EncryptionKeyArn", s.encryption_key_arn);
    if !s3.is_empty() {
        let mut monitoring = Map::new();
        monitoring.insert("S3MonitoringConfiguration".to_string(), Value::Map(s3));
        step.insert(
            "StepMonitoringConfiguration".to_string(),
            Value::Map(monitoring),
        );
    }
    Ok(Value::Map(step))
}

/// `emrutils.build_bootstrap_action`.
pub(crate) fn build_bootstrap_action(path: &str, name: &str, args: Option<Vec<String>>) -> Value {
    let mut ba = Map::new();
    apply(&mut ba, "Name", Some(Value::str(name)));
    let mut script = Map::new();
    apply(&mut script, "Args", args.map(|a| str_list(&a)));
    script.insert("Path".to_string(), Value::str(path));
    apply(&mut ba, "ScriptBootstrapAction", Some(Value::Map(script)));
    Value::Map(ba)
}

/// `emrutils.check_required_field`.
pub(crate) fn check_required_field(
    ctx: &Context,
    structure: &str,
    name: &str,
    value: Option<&Value>,
) -> Result<(), CliError> {
    if !value.is_some_and(Value::is_truthy) {
        return Err(missing_parameters(ctx, structure, name));
    }
    Ok(())
}

/// `emrutils.check_empty_string_list`.
pub(crate) fn check_empty_string_list(
    ctx: &Context,
    name: &str,
    value: Option<&Value>,
) -> Result<(), CliError> {
    let empty = match value {
        None => true,
        Some(v) if !v.is_truthy() => true,
        Some(Value::List(l)) => l.len() == 1 && l[0].as_str().is_some_and(|s| s.trim().is_empty()),
        _ => false,
    };
    if empty {
        return Err(param_error(
            ctx,
            &format!("The parameter {name} cannot be an empty list."),
        ));
    }
    Ok(())
}

/// `emrutils.join`.
pub(crate) fn join(values: &[String]) -> String {
    match values {
        [] => String::new(),
        [one] => one.clone(),
        _ => format!(
            "{} and {}",
            values[..values.len() - 1].join(", "),
            values[values.len() - 1]
        ),
    }
}

/// An `emr` client (`emrutils.call` / `get_client`).
pub(crate) fn emr_client(ctx: &Context) -> Result<Client<'_>, CliError> {
    Client::new(ctx, "emr")
}

/// `emrutils.call_and_display_response` (`CLIOperationCaller.invoke`).
pub(crate) fn call_and_display(
    ctx: &Context,
    operation: &str,
    params: Value,
) -> Result<(), CliError> {
    let client = emr_client(ctx)?;
    let result = client.call(operation, &params)?;
    display_response(ctx, operation, Response::Single(result))
}

/// `emrutils.display_response`.
pub(crate) fn display(ctx: &Context, command_name: &str, result: Value) -> Result<(), CliError> {
    display_response(ctx, command_name, Response::Single(result))
}

/// `emrutils.get_release_label`.
pub(crate) fn release_label(ctx: &Context, cluster_id: &str) -> Result<Option<String>, CliError> {
    let client = emr_client(ctx)?;
    let mut params = Map::new();
    params.insert("ClusterId".to_string(), Value::str(cluster_id));
    let result = client.call("DescribeCluster", &Value::Map(params))?;
    Ok(result
        .get("Cluster")
        .and_then(|c| c.get("ReleaseLabel"))
        .and_then(Value::as_str)
        .map(str::to_string))
}

/// `Command._apply_configs` for the configurations of `config.py`.
pub(crate) fn apply_configs(
    ctx: &Context,
    command: &str,
    defs: &[ArgDef],
    args: &mut Args,
) -> Result<(), CliError> {
    let configs = emr_configs(ctx);
    let supports = |name: &str| defs.iter().any(|d| d.name == name);
    let present = |args: &Args, key: &str| args.get(key).is_some_and(Value::is_truthy);
    let use_default_roles = command == "create-cluster" && args.bool("use_default_roles");
    for (name, arg_name, value_key) in [
        ("service_role", "service_role", None),
        ("log_uri", "log_uri", None),
        (
            "instance_profile",
            "ec2_attributes",
            Some("InstanceProfile"),
        ),
        ("key_name", "ec2_attributes", Some("KeyName")),
        ("enable_debugging", "enable_debugging", None),
        ("key_pair_file", "key_pair_file", None),
    ] {
        let Some(value) = configs.get(name) else {
            continue;
        };
        if use_default_roles && (name == "service_role" || name == "instance_profile") {
            continue;
        }
        let value = value.as_str().unwrap_or_default().to_string();
        if name == "enable_debugging" {
            if !(supports("enable-debugging") && supports("no-enable-debugging")) {
                continue;
            }
            if present(args, "enable_debugging") || present(args, "no_enable_debugging") {
                continue;
            }
            let (on, off) = match value.to_lowercase().as_str() {
                "true" => (true, false),
                "false" => (false, true),
                _ => {
                    let profile = match ctx.session.config().profile() {
                        Some(p) => format!("profile.{p}"),
                        None => "default".to_string(),
                    };
                    return Err(param_error(
                        ctx,
                        &format!(
                            "{value} for enable_debugging in the config file is invalid. The value should be either 'True' or 'False'. Use 'aws configure set {profile}.emr.enable_debugging <value>' command to set a valid value."
                        ),
                    ));
                }
            };
            args.values
                .insert("enable_debugging".to_string(), Value::Bool(on));
            args.values
                .insert("no_enable_debugging".to_string(), Value::Bool(off));
            continue;
        }
        if !supports(&arg_name.replace('_', "-")) {
            continue;
        }
        match value_key {
            None => {
                if present(args, arg_name) {
                    continue;
                }
                args.values.insert(arg_name.to_string(), Value::str(value));
            }
            Some(key) => {
                if present(args, arg_name)
                    && args
                        .get(arg_name)
                        .and_then(Value::as_map)
                        .is_some_and(|m| m.contains_key(key))
                {
                    continue;
                }
                if !present(args, arg_name) {
                    args.values.insert(arg_name.to_string(), Value::map());
                }
                if let Some(Value::Map(m)) = args.values.get_mut(arg_name) {
                    m.insert(key.to_string(), Value::str(value));
                }
            }
        }
    }
    Ok(())
}

/// `json.dumps(value, indent=indent)` (ASCII output, Python separators).
pub(crate) fn json_dumps(value: &Value, indent: usize) -> String {
    let mut out = String::new();
    dump(value, indent, 0, &mut out);
    out
}

fn dump(value: &Value, indent: usize, level: usize, out: &mut String) {
    let pad = |n: usize| " ".repeat(indent * n);
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(i) => out.push_str(&i.to_string()),
        Value::Float(f) => out.push_str(&crate::pyfmt::float_repr(*f)),
        Value::Str(s) => dump_str(s, out),
        Value::Bytes(b) => dump_str(&String::from_utf8_lossy(b), out),
        Value::List(l) => {
            if l.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, v) in l.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                out.push_str(&pad(level + 1));
                dump(v, indent, level + 1, out);
            }
            out.push('\n');
            out.push_str(&pad(level));
            out.push(']');
        }
        Value::Map(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                out.push_str(&pad(level + 1));
                dump_str(k, out);
                out.push_str(": ");
                dump(v, indent, level + 1, out);
            }
            out.push('\n');
            out.push_str(&pad(level));
            out.push('}');
        }
    }
}

fn dump_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `awscli.customizations.utils.get_policy_arn_suffix`.
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

/// A copy of the command's context whose clients use `endpoint_url`
/// (`session.create_client(..., endpoint_url=endpoint_url)`), for clients
/// a customization creates without `--endpoint-url`.
pub(crate) fn context_with_endpoint(
    ctx: &Context,
    endpoint_url: Option<String>,
) -> Result<Context, CliError> {
    let mut globals = ctx.globals.clone();
    globals.endpoint_url = endpoint_url;
    let session = crate::session::Session::new(&globals).map_err(|e| ctx.general_error(&e))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|e| general_error(ctx, &e.to_string()))?;
    Ok(Context {
        globals,
        session,
        runtime,
        terminal: crate::driver::Terminal {
            stdout_is_tty: ctx.terminal.stdout_is_tty,
            width: ctx.terminal.width,
        },
    })
}

/// botocore `json_decode_policies` (`after-call.iam`): the
/// `policyDocumentType` members of the output, URL-decoded and parsed.
pub(crate) fn decode_policies(client: &Client<'_>, operation: &str, value: &mut Value) {
    fn walk(shape: &crate::model::Shape<'_>, value: &mut Value) {
        match shape.type_name() {
            "structure" => {
                let Value::Map(m) = value else { return };
                for (name, member) in shape.members() {
                    let Some(v) = m.get_mut(name) else { continue };
                    if member.type_name() == "string" && member.name == "policyDocumentType" {
                        if let Value::Str(s) = v {
                            let decoded = percent_encoding::percent_decode_str(s)
                                .decode_utf8_lossy()
                                .into_owned();
                            if let Ok(parsed) = crate::cli::pyjson::loads(&decoded) {
                                *v = parsed;
                            }
                        }
                    } else {
                        walk(&member, v);
                    }
                }
            }
            "list" => {
                if let (Value::List(items), Some(member)) = (value, shape.list_member()) {
                    for item in items {
                        walk(&member, item);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(output) = client.model.operation(operation).and_then(|o| o.output()) {
        walk(&output, value);
    }
}
