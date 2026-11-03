//! `eks get-token` and `eks update-kubeconfig`
//! (`awscli/customizations/eks/get_token.py`, `update_kubeconfig.py`,
//! `kubeconfig.py`, `ordered_yaml.py`).

use std::path::Path;

use base64::Engine as _;
use chrono::{Duration, Utc};

use super::basic::{self, ArgDef};
use super::exec::Client;
use super::presign::{self, PresignRequest, presign_url};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::output::{self, Format, OutputOptions, Response};
use crate::session::sign::Credentials;
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "get-token" => get_token(ctx, args),
        "update-kubeconfig" => update_kubeconfig(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

const ALPHA_API: &str = "client.authentication.k8s.io/v1alpha1";
const BETA_API: &str = "client.authentication.k8s.io/v1beta1";
const V1_API: &str = "client.authentication.k8s.io/v1";

fn general(ctx: &Context, message: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("{message}"))
}

/// `GetTokenCommand._run_main`.
fn get_token(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = [
        ArgDef::new("cluster-name"),
        ArgDef::new("role-arn"),
        ArgDef::new("cluster-id"),
    ];
    let Some(a) = basic::parse(ctx, &["eks", "get-token"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let region = presign::region(ctx)?;
    let role_creds = match a.str("role-arn") {
        Some(role) => Some(assume_role(ctx, &role, "EKSGetTokenAuth")?),
        None => None,
    };
    if a.get("cluster-name").is_some() && a.get("cluster-id").is_some() {
        return Err(basic::param_validation(
            ctx,
            "The key \"cluster_id\" cannot be specified when one of the following keys are also specified: cluster_name",
        ));
    }
    let identifier = match (a.str("cluster-id"), a.str("cluster-name")) {
        (Some(id), _) if !id.is_empty() => id,
        (_, Some(name)) if !name.is_empty() => name,
        _ => {
            return Err(CliError::new(
                ExitCode::S3TransferFailed,
                "Either parameter --cluster-name or --cluster-id must be specified.\n",
            ));
        }
    };
    let creds = match role_creds {
        Some(c) => c,
        None => presign::credentials(ctx, region.as_deref())?,
    };
    let url = sts_presigned_url(ctx, region.as_deref(), &creds, &identifier, Utc::now())?;
    let token = token_from_url(&url);
    let expiration = (Utc::now() + Duration::minutes(14))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let api_version = discover_api_version(ctx.session.config().env("KUBERNETES_EXEC_INFO"));
    let mut status = Map::new();
    status.insert("expirationTimestamp".into(), Value::str(expiration));
    status.insert("token".into(), Value::str(token));
    let mut full = Map::new();
    full.insert("kind".into(), Value::str("ExecCredential"));
    full.insert("apiVersion".into(), Value::str(api_version));
    full.insert("spec".into(), Value::map());
    full.insert("status".into(), Value::Map(status));
    format_output(ctx, "get-token", Value::Map(full))?;
    basic::print("\n");
    Ok(ExitCode::Success)
}

/// `get_formatter(output, parsed_globals)(command_name, response)` on
/// stdout.
pub(crate) fn format_output(ctx: &Context, command: &str, value: Value) -> Result<(), CliError> {
    let name = ctx
        .globals
        .output
        .clone()
        .or_else(|| crate::config::ConfigLookup::config_variable(&ctx.session, "output"))
        .unwrap_or_else(|| "json".to_string());
    let Some(format) = Format::parse(&name) else {
        return Err(general(ctx, &format!("Unknown output type: {name}")));
    };
    let options = OutputOptions {
        format,
        query: ctx.globals.query.clone(),
        color: ctx.color(),
        terminal_width: ctx.terminal.width,
        stdout_is_tty: ctx.terminal.stdout_is_tty,
    };
    let mut buf = Vec::new();
    let result = output::write_response(command, Response::Single(value), &options, &mut buf);
    basic::print_bytes(&buf);
    result.map_err(|e| ctx.general_error(&e))
}

/// `TokenGenerator.get_token` from the presigned URL.
fn token_from_url(url: &str) -> String {
    let b64 = base64::engine::general_purpose::URL_SAFE.encode(url.as_bytes());
    format!("k8s-aws-v1.{}", b64.trim_end_matches('='))
}

/// `sts.generate_presigned_url('get_caller_identity', ...)` with the
/// `x-k8s-aws-id` header.
fn sts_presigned_url(
    ctx: &Context,
    region: Option<&str>,
    creds: &Credentials,
    identifier: &str,
    now: chrono::DateTime<Utc>,
) -> Result<String, CliError> {
    let endpoint = presign::resolve_endpoint(ctx, "sts", "GetCallerIdentity", None)?;
    let req = PresignRequest {
        method: "GET",
        url: format!("{}/", endpoint.trim_end_matches('/')),
        headers: vec![("x-k8s-aws-id".to_string(), identifier.to_string())],
        body: vec![
            ("Action".to_string(), "GetCallerIdentity".to_string()),
            ("Version".to_string(), "2011-06-15".to_string()),
        ],
    };
    Ok(presign_url(
        &req,
        creds,
        "sts",
        region.unwrap_or("us-east-1"),
        60,
        now,
    ))
}

/// `sts.assume_role(RoleArn=role, RoleSessionName=session)['Credentials']`.
fn assume_role(ctx: &Context, role: &str, session: &str) -> Result<Credentials, CliError> {
    let client = Client::new(ctx, "sts")?;
    let mut params = Map::new();
    params.insert("RoleArn".into(), Value::str(role));
    params.insert("RoleSessionName".into(), Value::str(session));
    let result = client.call("AssumeRole", &Value::Map(params))?;
    let c = result
        .get("Credentials")
        .ok_or_else(|| general(ctx, "'Credentials'"))?;
    let field = |k: &str| c.get(k).and_then(Value::as_str).map(str::to_string);
    Ok(Credentials {
        access_key: field("AccessKeyId").unwrap_or_default(),
        secret_key: field("SecretAccessKey").unwrap_or_default(),
        token: field("SessionToken"),
    })
}

/// `GetTokenCommand.discover_api_version`.
fn discover_api_version(exec_info: Option<&str>) -> String {
    let fallback = BETA_API;
    let raw = exec_info.unwrap_or_default();
    if raw.is_empty() {
        return fallback.to_string();
    }
    let Ok(info) = crate::cli::pyjson::loads(raw) else {
        basic::eprint(&format!(
            "Error parsing KUBERNETES_EXEC_INFO, defaulting to {fallback}. This is likely a bug in your Kubernetes client. Please update your Kubernetes client.\n"
        ));
        return fallback.to_string();
    };
    let version = info.get("apiVersion").and_then(Value::as_str);
    match version {
        Some(v) if v == V1_API || v == BETA_API => v.to_string(),
        Some(v) if v == ALPHA_API => {
            basic::eprint(&format!(
                "Kubeconfig user entry is using deprecated API version {v}. Run 'aws eks update-kubeconfig' to update.\n"
            ));
            v.to_string()
        }
        _ => {
            basic::eprint(&format!(
                "Unrecognized API version in KUBERNETES_EXEC_INFO, defaulting to {fallback}. This is likely due to an outdated AWS CLI. Please update your AWS CLI.\n"
            ));
            fallback.to_string()
        }
    }
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// `UpdateKubeconfigCommand._run_main`.
fn update_kubeconfig(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = [
        ArgDef::new("name").dest("cluster_name").required(),
        ArgDef::new("kubeconfig"),
        ArgDef::new("role-arn"),
        ArgDef::new("proxy-url"),
        ArgDef::new("dry-run")
            .flag()
            .default(crate::cli::argparse::NsVal::Bool(false)),
        ArgDef::new("verbose")
            .flag()
            .default(crate::cli::argparse::NsVal::Bool(false)),
        ArgDef::new("alias"),
        ArgDef::new("user-alias"),
        ArgDef::new("assume-role-arn"),
    ];
    let Some(a) = basic::parse(ctx, &["eks", "update-kubeconfig"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    if a.get("assume-role-arn").is_some() {
        return Err(crate::commands::not_implemented());
    }
    let cluster_name = a.str("cluster_name").unwrap_or_default();
    let client = Client::new(ctx, "eks")?;
    let mut params = Map::new();
    params.insert("name".into(), Value::str(&cluster_name));
    let full = client.call("DescribeCluster", &Value::Map(params))?;
    let cluster = match full.get("cluster") {
        Some(c @ Value::Map(m)) if !m.is_empty() && m.contains_key("status") => c.clone(),
        _ => return Err(general(ctx, "Cluster not found")),
    };
    let status = cluster
        .get("status")
        .map(crate::pyfmt::py_str)
        .unwrap_or_default();
    if status != "ACTIVE" && status != "UPDATING" {
        return Err(general(ctx, &format!("Cluster status is {status}")));
    }
    let get = |k: &str| cluster.get(k).cloned().unwrap_or(Value::Null);
    let cert = cluster
        .get("certificateAuthority")
        .and_then(|c| c.get("data"))
        .cloned()
        .unwrap_or_else(|| Value::str(""));
    let arn = get("arn");
    let mut cluster_inner = vec![
        ("certificate-authority-data", cert),
        ("server", get("endpoint")),
    ];
    if let Some(p) = a.str("proxy-url") {
        cluster_inner.push(("proxy-url", Value::str(p)));
    }
    let new_cluster = map(vec![("cluster", map(cluster_inner)), ("name", arn.clone())]);

    let arn_text = arn.as_str().unwrap_or_default().to_string();
    let region = arn_text
        .split(':')
        .nth(3)
        .ok_or_else(|| general(ctx, "list index out of range"))?;
    let outpost = cluster
        .get("outpostConfig")
        .filter(|v| !matches!(v, Value::Null));
    let (param, value) = match outpost {
        Some(o)
            if o.get("etcdInstanceType")
                .is_none_or(|v| matches!(v, Value::Null)) =>
        {
            ("--cluster-id", get("id"))
        }
        _ => ("--cluster-name", Value::str(&cluster_name)),
    };
    let mut exec_args = vec![
        Value::str("--region"),
        Value::str(region),
        Value::str("eks"),
        Value::str("get-token"),
        Value::str(param),
        value,
        Value::str("--output"),
        Value::str("json"),
    ];
    if let Some(role) = a.str("role-arn") {
        exec_args.push(Value::str("--role"));
        exec_args.push(Value::str(role));
    }
    let mut exec = vec![
        ("apiVersion", Value::str(BETA_API)),
        ("args", Value::List(exec_args)),
        ("command", Value::str("aws")),
    ];
    if let Some(profile) = ctx.session.config().profile() {
        exec.push((
            "env",
            Value::List(vec![map(vec![
                ("name", Value::str("AWS_PROFILE")),
                ("value", Value::str(profile)),
            ])]),
        ));
    }
    let user_name = match a.str("user-alias").filter(|s| !s.is_empty()) {
        Some(u) => Value::str(u),
        None => cluster
            .get("arn")
            .cloned()
            .unwrap_or_else(|| Value::str("")),
    };
    let new_user = map(vec![
        ("name", user_name),
        ("user", map(vec![("exec", map(exec))])),
    ]);

    let env = ctx
        .session
        .config()
        .env("KUBECONFIG")
        .unwrap_or_default()
        .to_string();
    let home = ctx.session.config().env("HOME").map(str::to_string);
    let paths = kubeconfig_paths(&env, a.str("kubeconfig").as_deref(), home.as_deref());
    let cluster_key = arn_text.clone();
    let mut config = None;
    for p in &paths {
        match load_kubeconfig(p) {
            Ok(c) if c.has_cluster(&cluster_key) => {
                config = Some(c);
                break;
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }
    let mut config = match config {
        Some(c) => c,
        None => load_kubeconfig(&paths[0]).map_err(|e| general(ctx, &e))?,
    };
    let updating = config.has_cluster(&cluster_key);
    let context = insert_cluster_user_pair(
        &mut config,
        new_cluster.clone(),
        new_user.clone(),
        a.str("alias"),
    )
    .map_err(|e| general(ctx, &e))?;
    let context_name = context
        .get("name")
        .map(crate::pyfmt::py_str)
        .unwrap_or_default();
    if a.bool("dry-run") {
        basic::print(&yaml_dump(&config.content));
    } else {
        write_kubeconfig(&config).map_err(|e| general(ctx, &e))?;
        if updating {
            basic::print(&format!(
                "Updated context {context_name} in {}\n",
                config.path
            ));
        } else {
            basic::print(&format!(
                "Added new context {context_name} to {}\n",
                config.path
            ));
        }
        if a.bool("verbose") {
            basic::print("Entries:\n\n");
            for entry in [&context, &new_user, &new_cluster] {
                basic::print(&yaml_dump(entry));
                basic::print("\n");
            }
        }
    }
    Ok(ExitCode::Success)
}

/// `ordered_yaml_dump` (`width = inf`).
fn yaml_dump(value: &Value) -> String {
    crate::output::yaml::yaml_dump_with(
        value,
        &crate::output::yaml::DumpOptions {
            sort_keys: false,
            yaml11_bools: false,
            timestamp_prefix: None,
            pure_python: false,
            width: usize::MAX / 4,
        },
    )
}

/// `KubeconfigSelector.__init__`: the candidate paths.
fn kubeconfig_paths(env: &str, path_in: Option<&str>, home: Option<&str>) -> Vec<String> {
    let default = "~/.kube/config";
    let expand = |p: &str| abspath(&expanduser(p, home));
    if let Some(p) = path_in {
        return vec![expand(p)];
    }
    let env = if env.is_empty() { default } else { env };
    let paths: Vec<String> = env
        .split(':')
        .filter(|e| !e.trim().is_empty())
        .map(expand)
        .collect();
    if paths.is_empty() {
        vec![expand(default)]
    } else {
        paths
    }
}

fn expanduser(p: &str, home: Option<&str>) -> String {
    match (p.strip_prefix('~'), home) {
        (Some(rest), Some(h)) if rest.is_empty() || rest.starts_with('/') => {
            format!("{}{rest}", h.trim_end_matches('/'))
        }
        _ => p.to_string(),
    }
}

/// `os.path.abspath`.
fn abspath(p: &str) -> String {
    let joined = if p.starts_with('/') {
        p.to_string()
    } else {
        let cwd = std::env::current_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        format!("{cwd}/{p}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

struct Kubeconfig {
    path: String,
    content: Value,
}

impl Kubeconfig {
    fn has_cluster(&self, name: &str) -> bool {
        match self.content.get("clusters") {
            Some(Value::List(l)) => l
                .iter()
                .any(|c| c.get("name").map(crate::pyfmt::py_str).as_deref() == Some(name)),
            _ => false,
        }
    }
}

fn new_content() -> Value {
    map(vec![
        ("apiVersion", Value::str("v1")),
        ("clusters", Value::List(vec![])),
        ("contexts", Value::List(vec![])),
        ("current-context", Value::str("")),
        ("kind", Value::str("Config")),
        ("preferences", Value::map()),
        ("users", Value::List(vec![])),
    ])
}

fn py_type(v: &Value) -> &'static str {
    match v {
        Value::Map(_) => "<class 'collections.OrderedDict'>",
        Value::List(_) => "<class 'list'>",
        Value::Str(_) => "<class 'str'>",
        Value::Int(_) => "<class 'int'>",
        Value::Float(_) => "<class 'float'>",
        Value::Bool(_) => "<class 'bool'>",
        Value::Null => "<class 'NoneType'>",
        Value::Bytes(_) => "<class 'bytes'>",
    }
}

/// `KubeconfigLoader.load_kubeconfig` with `KubeconfigValidator`.
fn load_kubeconfig(path: &str) -> Result<Kubeconfig, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(text) => match serde_yaml::from_str::<serde_yaml::Value>(&text) {
            Ok(v) => yaml_value(&v),
            Err(e) => return Err(format!("YamlError while loading kubeconfig: {e}")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(e) => {
            return Err(format!(
                "Can't open kubeconfig for reading: [Errno {}] {}: '{path}'",
                e.raw_os_error().unwrap_or(0),
                os_strerror(&e)
            ));
        }
    };
    let content = if matches!(content, Value::Null) {
        new_content()
    } else {
        content
    };
    let Value::Map(m) = &content else {
        return Err("Content not a <class 'dict'>.".to_string());
    };
    let reference = new_content();
    for (key, value) in reference.as_map().expect("map") {
        if let Some(v) = m.get(key)
            && !matches!(v, Value::Null)
            && std::mem::discriminant(v) != std::mem::discriminant(value)
        {
            return Err(format!(
                "{key} is wrong type: {} (Should be {})",
                py_type(v),
                py_type(value)
            ));
        }
    }
    for (key, _) in reference.as_map().expect("map") {
        if let Some(Value::List(l)) = m.get(key)
            && l.iter().any(|e| !matches!(e, Value::Map(_)))
        {
            return Err(format!(
                "Entry in {key} not a <class 'collections.OrderedDict'>. "
            ));
        }
    }
    Ok(Kubeconfig {
        path: path.to_string(),
        content,
    })
}

fn os_strerror(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
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
                        other => crate::pyfmt::py_str(&yaml_value(other)),
                    };
                    (key, yaml_value(v))
                })
                .collect(),
        ),
        serde_yaml::Value::Tagged(t) => yaml_value(&t.value),
    }
}

/// `KubeconfigAppender.insert_cluster_user_pair`.
fn insert_cluster_user_pair(
    config: &mut Kubeconfig,
    cluster: Value,
    user: Value,
    alias: Option<String>,
) -> Result<Value, String> {
    let cluster_name = cluster.get("name").cloned().unwrap_or(Value::Null);
    let user_name = user.get("name").cloned().unwrap_or(Value::Null);
    let name = match alias.filter(|a| !a.is_empty()) {
        Some(a) => Value::str(a),
        None => user_name.clone(),
    };
    let context = map(vec![
        (
            "context",
            map(vec![("cluster", cluster_name), ("user", user_name)]),
        ),
        ("name", name.clone()),
    ]);
    insert_entry(config, "clusters", cluster)?;
    insert_entry(config, "users", user)?;
    insert_entry(config, "contexts", context.clone())?;
    if let Value::Map(m) = &mut config.content {
        m.insert("current-context".into(), name);
    }
    Ok(context)
}

fn insert_entry(config: &mut Kubeconfig, key: &str, entry: Value) -> Result<(), String> {
    let Value::Map(m) = &mut config.content else {
        return Ok(());
    };
    let current = m.get(key).cloned().filter(Value::is_truthy);
    let entries = current.unwrap_or(Value::List(vec![]));
    let Value::List(mut list) = entries else {
        return Err(format!(
            "Tried to insert into {key}, which is a {} not a <class 'list'>",
            py_type(&entries)
        ));
    };
    let name = entry.get("name").cloned();
    let index = name.and_then(|n| list.iter().position(|e| e.get("name") == Some(&n)));
    match index {
        Some(i) => list[i] = entry,
        None => list.push(entry),
    }
    m.insert(key.to_string(), Value::List(list));
    Ok(())
}

/// `KubeconfigWriter.write_kubeconfig`.
fn write_kubeconfig(config: &Kubeconfig) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(dir) = Path::new(&config.path).parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Can't create directory for writing: {}", os_strerror(&e)))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&config.path)
        .map_err(|e| format!("Can't open kubeconfig for writing: {}", os_strerror(&e)))?;
    f.write_all(yaml_dump(&config.content).as_bytes())
        .map_err(|e| format!("Can't open kubeconfig for writing: {}", os_strerror(&e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn token_matches_botocore() {
        for (golden, token) in [
            (include_str!("testdata/eks_token.txt"), None),
            (include_str!("testdata/eks_token_session.txt"), Some("tok")),
        ] {
            let mut lines = golden.lines();
            let url = lines.next().unwrap();
            let expected_token = lines.next().unwrap();
            let region = url.split('.').nth(1).unwrap();
            let creds = Credentials {
                access_key: "AKIDEXAMPLE".to_string(),
                secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
                token: token.map(str::to_string),
            };
            let req = PresignRequest {
                method: "GET",
                url: format!("https://sts.{region}.amazonaws.com/"),
                headers: vec![("x-k8s-aws-id".to_string(), "my-cluster".to_string())],
                body: vec![
                    ("Action".to_string(), "GetCallerIdentity".to_string()),
                    ("Version".to_string(), "2011-06-15".to_string()),
                ],
            };
            let now = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
            let got = presign_url(&req, &creds, "sts", region, 60, now);
            assert_eq!(got, url);
            assert_eq!(token_from_url(&got), expected_token);
        }
    }

    #[test]
    fn paths() {
        assert_eq!(
            kubeconfig_paths("", None, Some("/h")),
            vec!["/h/.kube/config".to_string()]
        );
        assert_eq!(
            kubeconfig_paths("/a:: :/b/../c", None, Some("/h")),
            vec!["/a".to_string(), "/c".to_string()]
        );
    }
}
