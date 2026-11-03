//! `agent-toolkit get-skill-file` (`awscli/customizations/agenttoolkit/`).

use super::basic::{self, ArgDef};
use super::exec::{Client, sub_context};
use crate::driver::{Context, write_stdout};
use crate::error::{CliError, ExitCode};
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "get-skill-file" => get_skill_file(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

/// `agenttoolkit.utils.create_client`: the `us-east-1` client unless a
/// region is given on the command line.
fn client_context(ctx: &Context) -> Result<Option<Context>, CliError> {
    if ctx.globals.region.as_deref().is_some_and(|r| !r.is_empty()) {
        return Ok(None);
    }
    sub_context(
        ctx,
        ctx.globals.endpoint_url.clone(),
        Some("us-east-1".to_string()),
        None,
    )
    .map(Some)
}

/// `GetSkillFileCommand._run_main`.
fn get_skill_file(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        ArgDef::new("skill-name").required().type_name("string"),
        ArgDef::new("file-path").required().type_name("string"),
        ArgDef::new("skill-version").type_name("string"),
    ];
    let Some(a) = basic::parse(ctx, &["agent-toolkit", "get-skill-file"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let own = client_context(ctx)?;
    let client = Client::new(own.as_ref().unwrap_or(ctx), "agenttoolkit")?;
    let version = a
        .str("skill-version")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "latest".to_string());
    let mut params = Map::new();
    params.insert(
        "name".to_string(),
        Value::str(a.str("skill-name").unwrap_or_default()),
    );
    params.insert("skillVersion".to_string(), Value::str(version));
    params.insert(
        "filePath".to_string(),
        Value::str(a.str("file-path").unwrap_or_default()),
    );
    let response = client.call("GetSkillFile", &Value::Map(params))?;
    let body = match response.get("body") {
        Some(Value::Bytes(b)) => b.clone(),
        Some(Value::Str(s)) => s.clone().into_bytes(),
        _ => Vec::new(),
    };
    let text = String::from_utf8(body).map_err(|e| {
        ctx.general_error(&anyhow::anyhow!(
            "'utf-8' codec can't decode byte 0x{:02x} in position {}: invalid start byte",
            e.as_bytes()[e.utf8_error().valid_up_to()],
            e.utf8_error().valid_up_to()
        ))
    })?;
    write_stdout(ctx, text.as_bytes())?;
    Ok(ExitCode::Success)
}
