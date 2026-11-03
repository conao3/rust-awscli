//! `codecommit credential-helper` (`awscli/customizations/codecommit.py`).

use std::io::BufRead as _;

use chrono::{DateTime, Utc};

use super::basic::{self, ArgDef};
use super::presign;
use crate::cli::argparse::{Action, Nargs, NsVal, Parsed, Parser};
use crate::cli::render::argparse_error;
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::session::sign::{Credentials, SigV4};

pub(crate) fn run(ctx: &Context, name: &str, args: &[String]) -> Result<ExitCode, CliError> {
    match name {
        "credential-helper" => credential_helper(ctx, args),
        _ => Err(crate::commands::not_implemented()),
    }
}

/// `SubCommandArgParser.parse_known_args` for a command without
/// arguments of its own: the subcommand and its arguments, if one was
/// given.
pub(crate) fn parse_subcommand(
    ctx: &Context,
    args: &[String],
    choices: &[&str],
) -> Result<Option<(String, Vec<String>)>, CliError> {
    if args.len() == 1 && args[0] == "help" {
        return Ok(None);
    }
    let mut parser = Parser::new();
    let mut sub = Action::positional("subcommand", Nargs::Optional);
    sub.choices = Some(choices.iter().map(|c| c.to_string()).collect());
    parser.add(sub);
    let parsed = parser
        .parse_known_args(args)
        .map_err(|e| render(ctx, argparse_error(&e.to_string())))?;
    let Parsed::Args { ns, .. } = parsed else {
        unreachable!("no version action")
    };
    match ns.get("subcommand") {
        Some(NsVal::Str(s)) => {
            let mut rest = args.to_vec();
            if let Some(i) = rest.iter().position(|a| a == s) {
                rest.remove(i);
            }
            Ok(Some((s.clone(), rest)))
        }
        _ => Ok(None),
    }
}

fn render(ctx: &Context, f: crate::cli::Fail) -> CliError {
    crate::cli::render_fail(f, &ctx.globals, &ctx.session)
}

fn general(ctx: &Context, message: &str) -> CliError {
    ctx.general_error(&anyhow::anyhow!("{message}"))
}

/// `CodeCommitCommand`.
fn credential_helper(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let lineage = ["codecommit", "credential-helper"];
    match parse_subcommand(ctx, args, &["get", "store", "erase"])? {
        Some((sub, rest)) => {
            let defs: Vec<ArgDef> = if sub == "get" {
                vec![
                    ArgDef::new("ignore-host-check")
                        .flag()
                        .default(NsVal::Bool(false)),
                ]
            } else {
                vec![]
            };
            let Some(a) = basic::parse(
                ctx,
                &["codecommit", "credential-helper", &sub],
                &defs,
                &rest,
            )?
            else {
                return Ok(ExitCode::Success);
            };
            if sub == "get" {
                get(ctx, a.bool("ignore-host-check"))?;
            }
            Ok(ExitCode::Success)
        }
        None => {
            if basic::parse(ctx, &lineage, &[], args)?.is_none() {
                return Ok(ExitCode::Success);
            }
            Err(basic::usage_error(ctx, &lineage))
        }
    }
}

/// `CodeCommitGetCommand._run_main`.
fn get(ctx: &Context, ignore_host_check: bool) -> Result<(), CliError> {
    let mut params: Vec<(String, String)> = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|e| general(ctx, &e.to_string()))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(general(
                ctx,
                "not enough values to unpack (expected 2, got 1)",
            ));
        };
        match params.iter_mut().find(|(ek, _)| ek == k) {
            Some(e) => e.1 = v.to_string(),
            None => params.push((k.to_string(), v.to_string())),
        }
    }
    let param = |k: &str| {
        params
            .iter()
            .find(|(ek, _)| ek == k)
            .map(|(_, v)| v.clone())
            .ok_or_else(|| general(ctx, &format!("'{k}'")))
    };
    let host = param("host")?;
    if !(host.contains("amazon.com") || host.contains("amazonaws.com") || ignore_host_check) {
        return Ok(());
    }
    let url = format!("{}://{host}/{}", param("protocol")?, param("path")?);
    let region = extract_region(&host)
        .map(Ok)
        .unwrap_or_else(|| presign::region(ctx).map(Option::unwrap_or_default))?;
    let creds = match crate::driver::block_on(&ctx.runtime, ctx.session.credentials(Some(&region)))
    {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Err(general(
                ctx,
                "'NoneType' object has no attribute 'secret_key'",
            ));
        }
        Err(e) => return Err(ctx.session_error(&e)),
    };
    let signature = sign_request(&creds, &region, &url, Utc::now());
    let mut username = creds.access_key.clone();
    if let Some(token) = &creds.token {
        username.push('%');
        username.push_str(token);
    }
    basic::print(&format!("username={username}\npassword={signature}\n"));
    Ok(())
}

/// `CodeCommitGetCommand.extract_region` (the host part).
fn extract_region(host: &str) -> Option<String> {
    let re =
        regex::Regex::new(r"^(vpce-.+\.)?git-codecommit(-fips)?\.([^.]+)\.(vpce\.)?amazonaws\.com")
            .expect("regex");
    re.captures(host).map(|c| c[3].to_string())
}

/// `CodeCommitGetCommand.sign_request`.
fn sign_request(creds: &Credentials, region: &str, url: &str, now: DateTime<Utc>) -> String {
    let timestamp = now.format("%Y%m%dT%H%M%S").to_string();
    let split = crate::session::url::urlsplit(url);
    let hostname = split.netloc.split(':').next().unwrap_or_default();
    let canonical = format!("GIT\n{}\n\nhost:{hostname}\n\nhost\n", split.path);
    let signer = SigV4 {
        credentials: creds.clone(),
        service_name: "codecommit".to_string(),
        region_name: region.to_string(),
        s3: false,
    };
    let sts = signer.string_to_sign(&timestamp, &canonical);
    format!("{timestamp}Z{}", signer.signature(&timestamp, &sts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn region_and_signature() {
        assert_eq!(
            extract_region("git-codecommit.eu-west-1.amazonaws.com").as_deref(),
            Some("eu-west-1")
        );
        assert_eq!(
            extract_region("vpce-1.git-codecommit-fips.us-east-2.vpce.amazonaws.com").as_deref(),
            Some("us-east-2")
        );
        assert_eq!(extract_region("example.com"), None);
        let creds = Credentials {
            access_key: "AKIDEXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
            token: None,
        };
        let now = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let sig = sign_request(
            &creds,
            "eu-west-1",
            "https://git-codecommit.eu-west-1.amazonaws.com/v1/repos/r",
            now,
        );
        assert_eq!(
            sig,
            include_str!("testdata/codecommit_signature.txt").trim_end()
        );
    }
}
