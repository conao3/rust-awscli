//! `ecr get-login-password` and `ecr-public get-login-password`
//! (`awscli/customizations/ecr.py`, `ecr_public.py`).

use base64::Engine as _;

use super::basic;
use super::exec::Client;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::value::Value;

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    run_service(ctx, "ecr", name, args)
}

pub(crate) fn run_public(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    run_service(ctx, "ecr-public", name, args)
}

fn run_service(
    ctx: &Context,
    service: &str,
    name: &str,
    args: &[String],
) -> Result<ExitCode, CliError> {
    match name {
        "get-login-password" => get_login_password(ctx, service, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

/// `ECRGetLoginPassword._run_main` / `ECRPublicGetLoginPassword._run_main`.
fn get_login_password(ctx: &Context, service: &str, args: &[String]) -> Result<ExitCode, CliError> {
    if basic::parse(ctx, &[service, "get-login-password"], &[], args)?.is_none() {
        return Ok(ExitCode::Success);
    }
    let client = Client::new(ctx, service)?;
    let result = client.call("GetAuthorizationToken", &Value::map())?;
    let auth = match service {
        "ecr" => result
            .get("authorizationData")
            .and_then(|d| d.as_list())
            .and_then(|l| l.first())
            .ok_or_else(|| python_error(ctx, "IndexError: list index out of range"))?,
        _ => result
            .get("authorizationData")
            .ok_or_else(|| python_error(ctx, "'authorizationData'"))?,
    };
    let token = auth
        .get("authorizationToken")
        .and_then(Value::as_str)
        .ok_or_else(|| python_error(ctx, "'authorizationToken'"))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(token)
        .map_err(|_| python_error(ctx, "Incorrect padding"))?;
    let text = String::from_utf8_lossy(&decoded).into_owned();
    let parts: Vec<&str> = text.split(':').collect();
    let [_, password] = parts.as_slice() else {
        return Err(python_error(
            ctx,
            &format!(
                "{} values to unpack (expected 2, got {})",
                if parts.len() > 2 {
                    "too many"
                } else {
                    "not enough"
                },
                parts.len()
            ),
        ));
    };
    basic::print(&format!("{password}\n"));
    Ok(ExitCode::Success)
}

fn python_error(ctx: &Context, message: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("{message}"))
}
