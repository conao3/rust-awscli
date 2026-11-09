//! `aws sso login` and `aws sso logout`
//! (`awscli/customizations/sso/login.py`, `logout.py`).

pub(crate) mod flow;

use crate::cli::Fail;
use crate::commands::configure::basic::{self, ArgSpec, ArgsExt, CliType, Outcome};
use crate::commands::configure::writer::{NewValues, WriterValue};
use crate::commands::configure::{fail, out, writer_error};
use crate::config::ini::IniValue;
use crate::driver::{Context, block_on};
use crate::error::{CliError, ExitCode};
use crate::session::cache;
use crate::session::client::ApiClient;
use crate::session::error::{ErrorKind, SessionError};
use crate::value::{Map, Value};

/// `LOGIN_ARGS` (`--no-browser`, `--use-device-code`, `--redirect-port`).
pub(crate) fn login_args() -> Vec<ArgSpec> {
    let mut port = ArgSpec::option("redirect-port");
    port.cli_type = CliType::Integer;
    vec![
        ArgSpec::flag("no-browser"),
        ArgSpec::flag("use-device-code"),
        port,
    ]
}

/// `validate_redirect_port`.
pub(crate) fn validate_redirect_port(
    ctx: &Context,
    args: &basic::Args,
) -> Result<Option<u16>, CliError> {
    match args.get("redirect_port").and_then(basic::ArgValue::as_int) {
        None => Ok(None),
        Some(p) if (1..=65535).contains(&p) => Ok(Some(p as u16)),
        Some(_) => Err(fail(
            ctx,
            Fail::ParamValidation(
                "Invalid value for --redirect-port. Value must be between 1 and 65535.".into(),
            ),
        )),
    }
}

/// The SSO configuration of `BaseSSOCommand._get_sso_config`.
pub(crate) struct SsoConfig {
    pub start_url: String,
    pub region: Option<String>,
    pub session_name: Option<String>,
    pub registration_scopes: Option<Vec<String>>,
}

/// `parse_sso_registration_scopes`.
pub(crate) fn parse_scopes(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn configuration(msg: impl Into<String>) -> SessionError {
    SessionError::new(ErrorKind::Configuration, msg)
}

/// `BaseSSOCommand._get_sso_config`.
pub(crate) fn get_sso_config(
    ctx: &Context,
    sso_session: Option<String>,
) -> Result<SsoConfig, SessionError> {
    let config = ctx.session.config();
    let scoped = config.scoped_config()?;
    let session = sso_session.or_else(|| {
        scoped
            .get("sso_session")
            .and_then(IniValue::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    });
    let (section, name) = match &session {
        Some(name) => {
            let Some(section) = config.full_config().sso_sessions.get(name) else {
                return Err(configuration(format!(
                    "The specified sso-session does not exist: \"{name}\""
                )));
            };
            (section, Some(name.clone()))
        }
        None => (scoped, None),
    };
    let missing: Vec<&str> = ["sso_start_url", "sso_region"]
        .into_iter()
        .filter(|k| !section.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        let mut msg = format!(
            "Missing the following required SSO configuration values: {}. ",
            missing.join(", ")
        );
        if name.is_none() {
            msg.push_str(
                "To make sure this profile is properly configured to use SSO, please run: aws configure sso",
            );
        }
        return Err(configuration(msg));
    }
    let get = |k: &str| {
        section
            .get(k)
            .and_then(IniValue::as_str)
            .map(str::to_string)
    };
    Ok(SsoConfig {
        start_url: get("sso_start_url").unwrap_or_default(),
        region: get("sso_region"),
        registration_scopes: if name.is_some() {
            get("sso_registration_scopes").map(|s| parse_scopes(&s))
        } else {
            None
        },
        session_name: name,
    })
}

fn parse(
    ctx: &Context,
    args: &[String],
    table: &[ArgSpec],
) -> Result<Option<basic::Args>, CliError> {
    match basic::parse_args(args, table, &[]).map_err(|f| fail(ctx, f))? {
        Outcome::Help => {
            out("sso\n");
            Ok(None)
        }
        Outcome::Run(a) => Ok(Some(a)),
    }
}

/// `aws sso login` (`LoginCommand`).
pub(crate) fn login(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let mut table = login_args();
    table.push(ArgSpec::option("sso-session"));
    let Some(args) = parse(ctx, args, &table)? else {
        return Ok(ExitCode::Success);
    };
    let redirect_port = validate_redirect_port(ctx, &args)?;
    let sso =
        get_sso_config(ctx, args.str_arg("sso_session")).map_err(|e| ctx.session_error(&e))?;
    let config = ctx.session.config();
    let (resolved_url, region) = block_on(
        &ctx.runtime,
        flow::resolve_start_url(config, &sso.start_url, sso.region.as_deref()),
    )
    .map_err(|e| ctx.session_error(&e))?;
    if resolved_url != sso.start_url {
        crate::session::useragent::register_command(&[
            crate::session::useragent::Feature::SsoLoginVanityUrl,
        ]);
    }
    let login = flow::SsoLogin {
        sso_region: region.clone(),
        start_url: sso.start_url.clone(),
        resolved_start_url: Some(resolved_url),
        session_name: sso.session_name.clone(),
        registration_scopes: sso.registration_scopes.clone(),
        force_refresh: true,
        use_device_code: args.flag("use_device_code"),
        redirect_port,
        handler: if args.flag("no_browser") {
            flow::PendingHandler::PrintOnly
        } else {
            flow::PendingHandler::OpenBrowser
        },
    };
    block_on(&ctx.runtime, flow::do_sso_login(config, &login))
        .map_err(|e| ctx.session_error(&e))?;
    if sso.region.as_deref() != Some(region.as_str()) {
        let section = match &sso.session_name {
            Some(name) => format!("sso-session {name}"),
            None => format!(
                "profile {}",
                config.profile().unwrap_or_else(|| "default".to_string())
            ),
        };
        let mut nv = NewValues::new();
        nv.insert("sso_region".into(), WriterValue::Str(region.clone()));
        let path = crate::cli::paramfile::expanduser(
            &config.get("config_file").ok().flatten().unwrap_or_default(),
        );
        crate::commands::configure::writer::update_config(Some(&section), &nv, &path, false)
            .map_err(writer_error)?;
        out(&format!("SSO region updated to {region}\n"));
    }
    out(&format!(
        "Successfully logged into Start URL: {}\n",
        sso.start_url
    ));
    Ok(ExitCode::Success)
}

/// `aws sso logout` (`LogoutCommand`).
pub(crate) fn logout(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    if parse(ctx, args, &[])?.is_none() {
        return Ok(ExitCode::Success);
    }
    let config = ctx.session.config();
    let token_dir = cache::sso_cache(config);
    for (path, contents) in json_files(token_dir.dir()) {
        if contents.get("accessToken").is_none() {
            continue;
        }
        if let Some(region) = contents
            .get("region")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            let client = ApiClient::new(config, "sso", Some(region.to_string()), None, None)
                .map_err(|e| ctx.session_error(&e))?;
            let mut params = Map::new();
            params.insert(
                "accessToken".into(),
                contents.get("accessToken").cloned().unwrap_or(Value::Null),
            );
            match block_on(
                &ctx.runtime,
                client.call("Logout", &Value::Map(params), None),
            ) {
                Ok(_) | Err(crate::session::client::ApiError::Service(_)) => {}
                Err(crate::session::client::ApiError::Session(e)) => {
                    return Err(ctx.session_error(&e));
                }
            }
        }
        let _ = std::fs::remove_file(path);
    }
    let creds_dir = cache::cli_cache(config);
    for (path, contents) in json_files(creds_dir.dir()) {
        if contents.get("ProviderType").and_then(Value::as_str) == Some("sso") {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(ExitCode::Success)
}

/// The JSON files of a cache directory (`os.listdir` + `json.load`,
/// skipping files that fail to load).
pub(crate) fn json_files(dir: &std::path::Path) -> Vec<(std::path::PathBuf, Value)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(v) = crate::cli::pyjson::loads(&text) {
            out.push((path, v));
        }
    }
    out
}
