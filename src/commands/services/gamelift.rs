//! `gamelift upload-build` and `gamelift get-game-session-log`
//! (`awscli/customizations/gamelift/`).

use std::path::Path;

use super::basic::{self, ArgDef};
use super::deploy::zipfile::{ZipWriter, walk};
use super::exec::{Client, sub_context};
use crate::cli::argparse::Nargs;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::value::{Map, Value};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "upload-build" => upload_build(ctx, args),
        "get-game-session-log" => get_game_session_log(ctx, args),
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

fn key_error(ctx: &Context, key: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("'{key}'"))
}

fn get_str(ctx: &Context, v: &Value, key: &str) -> Result<String, CliError> {
    v.get(key)
        .map(crate::pyfmt::py_str)
        .ok_or_else(|| key_error(ctx, key))
}

/// `human_readable_size` (`awscli/customizations/s3/utils.py`).
pub(crate) fn human_readable_size(value: f64) -> String {
    const SUFFIXES: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let base = 1024f64;
    if value == 1.0 {
        return "1 Byte".to_string();
    }
    if value < base {
        return format!("{} Bytes", value.trunc() as i64);
    }
    for (i, suffix) in SUFFIXES.iter().enumerate() {
        let unit = base.powi(i as i32 + 2);
        if ((value / unit) * base).round_ties_even() < base {
            return format!("{:.1} {suffix}", base * value / unit);
        }
    }
    String::new()
}

/// `parse_tags`.
fn parse_tags(raw: &[String]) -> Value {
    Value::List(
        raw.iter()
            .map(|t| {
                let (k, v) = t.split_once('=').unwrap_or((t, ""));
                map(vec![("Key", Value::str(k)), ("Value", Value::str(v))])
            })
            .collect(),
    )
}

/// `validate_directory`.
fn validate_directory(root: &str) -> bool {
    !root.is_empty()
        && walk(Path::new(root), &|_| true)
            .iter()
            .any(|(_, _, files)| !files.is_empty())
}

/// `zip_directory`.
fn zip_directory(root: &str) -> std::io::Result<Vec<u8>> {
    let source_root = super::deploy::cmds::abspath(root);
    let mut zip = ZipWriter::new();
    for (dir, _, files) in walk(Path::new(&source_root), &|_| true) {
        for f in files {
            let full = dir.join(&f);
            let full_text = full.display().to_string();
            let relative = full_text
                .strip_prefix(&format!("{source_root}/"))
                .unwrap_or(&full_text)
                .to_string();
            zip.write_file(&full, &relative)?;
        }
    }
    Ok(zip.finish())
}

/// `UploadBuildCommand._run_main`.
fn upload_build(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        ArgDef::new("name").required(),
        ArgDef::new("build-version").required(),
        ArgDef::new("build-root").required(),
        ArgDef::new("server-sdk-version"),
        ArgDef::new("operating-system"),
        ArgDef::new("tags").nargs(Nargs::OneOrMore),
    ];
    let Some(a) = basic::parse(ctx, &["gamelift", "upload-build"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let gamelift = Client::new(ctx, "gamelift")?;
    let build_root = a.str("build-root").unwrap_or_default();
    if !validate_directory(&build_root) {
        return Err(CliError::new(
            ExitCode::General,
            format!(
                "Fail to upload {build_root}. The build root directory is empty or does not exist.\n"
            ),
        ));
    }
    let mut params = vec![
        ("Name", Value::str(a.str("name").unwrap_or_default())),
        (
            "Version",
            Value::str(a.str("build-version").unwrap_or_default()),
        ),
    ];
    if let Some(os) = a.str("operating-system").filter(|s| !s.is_empty()) {
        params.push(("OperatingSystem", Value::str(os)));
    }
    if let Some(v) = a.str("server-sdk-version").filter(|s| !s.is_empty()) {
        params.push(("ServerSdkVersion", Value::str(v)));
    }
    if let Some(tags) = a.str_list("tags").filter(|t| !t.is_empty()) {
        params.push(("Tags", parse_tags(&tags)));
    }
    let response = gamelift.call("CreateBuild", &map(params))?;
    let build = response
        .get("Build")
        .ok_or_else(|| key_error(ctx, "Build"))?;
    let build_id = get_str(ctx, build, "BuildId")?;
    let response = gamelift.call(
        "RequestUploadCredentials",
        &map(vec![("BuildId", Value::str(&build_id))]),
    )?;
    let creds = response
        .get("UploadCredentials")
        .ok_or_else(|| key_error(ctx, "UploadCredentials"))?;
    let location = response
        .get("StorageLocation")
        .ok_or_else(|| key_error(ctx, "StorageLocation"))?;
    let bucket = get_str(ctx, location, "Bucket")?;
    let key = get_str(ctx, location, "Key")?;
    let access_key = get_str(ctx, creds, "AccessKeyId")?;
    let secret_key = get_str(ctx, creds, "SecretAccessKey")?;
    let token = get_str(ctx, creds, "SessionToken")?;
    let s3_ctx = sub_context(ctx, None, None, Some((access_key, secret_key, Some(token))))?;
    let s3 = Client::with_config(
        &s3_ctx,
        "s3",
        crate::driver::ClientConfig {
            explicit_credentials: true,
            ..Default::default()
        },
    )?;
    let bundle = zip_directory(&build_root).map_err(|e| ctx.general_error(&e.into()))?;
    let size = bundle.len() as f64;
    s3.call(
        "PutObject",
        &map(vec![
            ("Bucket", Value::str(&bucket)),
            ("Key", Value::str(&key)),
            ("Body", Value::Bytes(bundle)),
        ]),
    )?;
    if size > 0.0 {
        let line = format!(
            "\rUploading {build_root}:  {} / {}  ({:.2}%)",
            human_readable_size(size),
            human_readable_size(size),
            100.0
        );
        basic::print(&line);
        basic::flush();
        basic::print(&line);
        basic::flush();
    }
    basic::print(&format!(
        "Successfully uploaded {build_root} to AWS GameLift\nBuild ID: {build_id}\n"
    ));
    Ok(ExitCode::Success)
}

/// `GetGameSessionLogCommand._run_main`.
fn get_game_session_log(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let defs = vec![
        ArgDef::new("game-session-id").required(),
        ArgDef::new("save-as").required(),
    ];
    let Some(a) = basic::parse(ctx, &["gamelift", "get-game-session-log"], &defs, args)? else {
        return Ok(ExitCode::Success);
    };
    let client = Client::new(ctx, "gamelift")?;
    let id = a.str("game-session-id").unwrap_or_default();
    let response = client.call(
        "GetGameSessionLogUrl",
        &map(vec![("GameSessionId", Value::str(&id))]),
    )?;
    let url = get_str(ctx, &response, "PreSignedUrl")?;
    let fetched = crate::driver::block_on(&ctx.runtime, async {
        let r = reqwest::get(&url).await?;
        let status = r.status();
        let body = r.bytes().await?;
        Ok::<_, reqwest::Error>((status, body))
    });
    let body = match fetched {
        Ok((status, body)) if status.as_u16() < 400 => body,
        Ok((status, _)) => {
            return Err(ctx.general_error(&anyhow::anyhow!(
                "HTTP Error {}: {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or_default()
            )));
        }
        Err(e) => {
            return Err(ctx.general_error(&anyhow::anyhow!("<urlopen error {e}>")));
        }
    };
    basic::print(&format!(
        "Downloading log archive for game session {id}...\r"
    ));
    let save_as = a.str("save-as").unwrap_or_default();
    std::fs::write(&save_as, &body).map_err(|e| {
        ctx.general_error(&anyhow::anyhow!(
            "{}",
            crate::cli::paramfile::os_error(&e, &save_as)
        ))
    })?;
    basic::print(&format!(
        "Successfully downloaded log archive for game session {id} to {save_as}\n"
    ));
    Ok(ExitCode::Success)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human_readable_size(1.0), "1 Byte");
        assert_eq!(human_readable_size(10.0), "10 Bytes");
        assert_eq!(human_readable_size(1024.0), "1.0 KiB");
        assert_eq!(human_readable_size(1048576.0), "1.0 MiB");
        assert_eq!(human_readable_size(1000000.0), "976.6 KiB");
    }
}
