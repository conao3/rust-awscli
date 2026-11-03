//! `lightsail push-container-image`
//! (`awscli/customizations/lightsail/push_container_image.py`).

use std::io::Write as _;
use std::process::{Command, Stdio};

use super::basic::{self, ArgDef, Args};
use crate::cli::GlobalArgs;
use crate::config::ConfigLookup;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "push-container-image" => push_container_image(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

/// `int(value)` of a timeout, `None` for `0` (`_resolve_timeout`).
fn timeout(value: Option<&str>) -> Option<i64> {
    let v = value
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(60);
    (v != 0).then_some(v)
}

/// `PushContainerImage._get_input_request`.
fn input_request(a: &Args, globals: &GlobalArgs, config: &dyn ConfigLookup) -> Value {
    let mut payload = Map::new();
    for (key, arg) in [
        ("service", "service-name"),
        ("image", "image"),
        ("label", "label"),
    ] {
        payload.insert(key.to_string(), Value::str(a.str(arg).unwrap_or_default()));
    }
    let mut c = Map::new();
    c.insert("debug".to_string(), Value::Bool(globals.debug));
    if let Some(e) = &globals.endpoint_url {
        c.insert("endpoint".to_string(), Value::str(e));
    }
    if globals.no_verify_ssl {
        c.insert("doNotVerifySSL".to_string(), Value::Bool(true));
    } else if globals.ca_bundle.as_deref().is_some_and(|b| !b.is_empty()) {
        c.insert("doNotVerifySSL".to_string(), Value::Bool(false));
    }
    c.insert("paginate".to_string(), Value::Bool(!globals.no_paginate));
    if let Some(o) = &globals.output {
        c.insert("output".to_string(), Value::str(o));
    }
    if let Some(q) = &globals.query {
        c.insert("query".to_string(), Value::str(q));
    }
    if let Some(p) = globals
        .profile
        .clone()
        .filter(|p| !p.is_empty())
        .or_else(|| config.config_variable("profile").filter(|p| !p.is_empty()))
    {
        c.insert("profile".to_string(), Value::str(p));
    }
    if let Some(r) = globals
        .region
        .clone()
        .filter(|r| !r.is_empty())
        .or_else(|| config.config_variable("region").filter(|r| !r.is_empty()))
    {
        c.insert("region".to_string(), Value::str(r));
    }
    c.insert(
        "doNotSignRequest".to_string(),
        Value::Bool(globals.no_sign_request),
    );
    if let Some(b) = globals
        .ca_bundle
        .clone()
        .filter(|b| !b.is_empty())
        .or_else(|| {
            config
                .config_variable("ca_bundle")
                .filter(|b| !b.is_empty())
        })
    {
        c.insert("caBundle".to_string(), Value::str(b));
    }
    if let Some(t) = timeout(globals.cli_read_timeout.as_deref()) {
        c.insert("readTimeout".to_string(), Value::int(t));
    }
    if let Some(t) = timeout(globals.cli_connect_timeout.as_deref()) {
        c.insert("connectTimeout".to_string(), Value::int(t));
    }
    if let Some(f) = &globals.cli_binary_format {
        c.insert("cliBinaryFormat".to_string(), Value::str(f));
    }
    c.insert(
        "cliVersion".to_string(),
        Value::str(crate::model::REFERENCE_VERSION.trim()),
    );
    let mut request = Map::new();
    request.insert("inputVersion".to_string(), Value::str("1"));
    request.insert("operation".to_string(), Value::str("PushContainerImage"));
    request.insert("payload".to_string(), Value::Map(payload));
    request.insert("configuration".to_string(), Value::Map(c));
    Value::Map(request)
}

/// `PushContainerImage._run_main`.
fn push_container_image(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        ArgDef::new("service-name").required(),
        ArgDef::new("image").required(),
        ArgDef::new("label").required(),
    ];
    let Some(a) = basic::parse(ctx, &["lightsail", "push-container-image"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    if ctx.globals.query.is_some() {
        return Err(ctx.general_error(&anyhow::anyhow!(
            "Object of type ParsedResult is not JSON serializable"
        )));
    }
    let payload = crate::pyfmt::json_dumps_default(&input_request(&a, &ctx.globals, &ctx.session));
    let argv = ["lightsailctl", "--plugin", "--input-stdin"];
    let child = Command::new(argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ctx.general_error(&anyhow::anyhow!(
                "The Lightsail Control (lightsailctl) plugin was not found. To download and install it, see https://lightsail.aws.amazon.com/ls/docs/en_us/articles/amazon-lightsail-install-software"
            )));
        }
        Err(e) => return Err(ctx.general_error(&e.into())),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes());
    }
    let status = child.wait().map_err(|e| ctx.general_error(&e.into()))?;
    if !status.success() {
        let code = match status.code() {
            Some(c) => c.to_string(),
            None => {
                use std::os::unix::process::ExitStatusExt as _;
                format!("-{}", status.signal().unwrap_or(0))
            }
        };
        let quoted: Vec<String> = argv.iter().map(|a| format!("'{a}'")).collect();
        return Err(ctx.general_error(&anyhow::anyhow!(
            "Command '[{}]' returned non-zero exit status {code}.",
            quoted.join(", ")
        )));
    }
    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn payload() {
        let mut values = indexmap::IndexMap::new();
        for (k, v) in [("service_name", "svc"), ("image", "img:1"), ("label", "l")] {
            values.insert(k.to_string(), Value::str(v));
        }
        let a = Args { values };
        let globals = GlobalArgs {
            region: Some("us-west-2".to_string()),
            ..Default::default()
        };
        let config: HashMap<String, String> = HashMap::new();
        assert_eq!(
            crate::pyfmt::json_dumps_default(&input_request(&a, &globals, &config)),
            format!(
                "{{\"inputVersion\": \"1\", \"operation\": \"PushContainerImage\", \"payload\": {{\"service\": \"svc\", \"image\": \"img:1\", \"label\": \"l\"}}, \"configuration\": {{\"debug\": false, \"paginate\": true, \"region\": \"us-west-2\", \"doNotSignRequest\": false, \"readTimeout\": 60, \"connectTimeout\": 60, \"cliVersion\": \"{}\"}}}}",
                crate::model::REFERENCE_VERSION.trim()
            )
        );
    }
}
