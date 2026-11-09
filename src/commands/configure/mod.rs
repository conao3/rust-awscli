//! `aws configure` (`awscli/customizations/configure/`): the interactive
//! prompts, `get`, `set`, `list`, `list-profiles`, `export-credentials`,
//! `import`, `add-model`, `mfa-login`, `sso`, and `sso-session`.

pub(crate) mod basic;
mod sso;
pub(crate) mod writer;

use std::io::{IsTerminal, Write};

use basic::{ArgSpec, Args, ArgsExt, CliType, Outcome};
use writer::{NewValues, WriterValue};

use crate::cli::Fail;
use crate::config::Config;
use crate::config::ini::{IniValue, Section};
use crate::driver::Context;
use crate::error::{CliError, ExitCode};
use crate::output::errors::render_general_error;
use crate::session::credentials::{self, LoadedCredentials};
use crate::session::error::{ErrorKind, SessionError};
use crate::session::prompt::{self, PromptError};
use crate::value::Value;

pub(crate) const SUBCOMMANDS: &[&str] = &[
    "list",
    "get",
    "set",
    "add-model",
    "import",
    "list-profiles",
    "sso",
    "sso-session",
    "mfa-login",
    "export-credentials",
    "agent-toolkit",
    "wizard",
];

pub(crate) const NOT_SET: &str = "<not set>";

/// Write to stdout (`uni_print` / `sys.stdout.write`).
pub(crate) fn out(text: &str) {
    crate::output::stdout::write_flushed(text.as_bytes());
}

/// Write to stderr.
pub(crate) fn err(text: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.flush();
}

pub(crate) fn fail(ctx: &Context, f: Fail) -> CliError {
    crate::cli::render::render(f, Some(&ctx.globals), Some(&ctx.session))
}

/// `GeneralExceptionHandler`: `str(exception)` with exit 255.
pub(crate) fn general(message: &str) -> CliError {
    CliError::new(ExitCode::General, render_general_error(message))
}

/// `ConfigurationErrorHandler`.
pub(crate) fn configuration_error(ctx: &Context, message: &str) -> CliError {
    ctx.session_error(&SessionError::new(ErrorKind::Configuration, message))
}

pub(crate) fn prompt_error(e: PromptError) -> CliError {
    match e {
        PromptError::Interrupted => {
            out("\n");
            CliError::new(ExitCode::Interrupted, "")
        }
        other => general(other.message()),
    }
}

pub(crate) fn writer_error(e: writer::WriterError) -> CliError {
    general(&e.0)
}

/// `PERMISSIONS_WARNING_TEMPLATE` / `warn_if_permissive`.
pub(crate) fn warn_if_permissive(path: &str) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if !meta.is_file() {
        return;
    }
    if meta.permissions().mode() & 0o777 & !0o700 != 0 {
        err(&format!(
            "\nawsr: [WARNING]: The file '{path}' is accessible by other users. Consider running 'chmod 600 {path}' to restrict access to only your user.\n"
        ));
    }
}

/// `mask_value`.
pub(crate) fn mask_value(value: Option<&str>) -> String {
    match value {
        None => "None".to_string(),
        Some(v) => {
            let chars: Vec<char> = v.chars().collect();
            let tail: String = chars[chars.len().saturating_sub(4)..].iter().collect();
            format!("{}{tail}", "*".repeat(16))
        }
    }
}

/// `shlex.quote`.
fn shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// `get_section_header`.
pub(crate) fn get_section_header(section_type: &str, name: &str) -> String {
    let name = if name.contains([' ', '\t']) {
        shlex_quote(name)
    } else {
        name.to_string()
    };
    format!("{section_type} {name}")
}

/// `profile_to_section`.
pub(crate) fn profile_to_section(profile: &str) -> String {
    if profile == "default" {
        profile.to_string()
    } else {
        get_section_header("profile", profile)
    }
}

/// `PREDEFINED_SECTION_NAMES = 'plugins'` is a string, so membership is a
/// substring test.
fn is_predefined(name: &str) -> bool {
    "plugins".contains(name)
}

fn config_file(config: &Config) -> String {
    crate::cli::paramfile::expanduser(&config.get("config_file").ok().flatten().unwrap_or_default())
}

fn credentials_file(config: &Config) -> String {
    crate::cli::paramfile::expanduser(
        &config
            .get("credentials_file")
            .ok()
            .flatten()
            .unwrap_or_default(),
    )
}

fn update(
    section: Option<&str>,
    values: &NewValues,
    path: &str,
    check_permissions: bool,
) -> Result<(), CliError> {
    writer::update_config(section, values, path, check_permissions).map_err(writer_error)
}

/// Help text for `aws configure ... help` (the reference renders the
/// reStructuredText man page).
fn help(name: &str) -> Result<ExitCode, CliError> {
    out(&format!("{name}\n"));
    Ok(ExitCode::Success)
}

/// `aws configure ...`.
pub(crate) fn run(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    if let Some((sub, rest)) =
        basic::parse_subcommand(args, &[], SUBCOMMANDS).map_err(|f| fail(ctx, f))?
    {
        return match sub.as_str() {
            "list" => list(ctx, &rest),
            "get" => get(ctx, &rest),
            "set" => set(ctx, &rest),
            "add-model" => add_model(ctx, &rest),
            "import" => import(ctx, &rest),
            "list-profiles" => list_profiles(ctx, &rest),
            "sso" => sso::configure_sso(ctx, &rest),
            "sso-session" => sso::configure_sso_session(ctx, &rest),
            "mfa-login" => mfa_login(ctx, &rest),
            "export-credentials" => export_credentials(ctx, &rest),
            "wizard" => crate::commands::services::wizard::run(ctx, "configure", &rest),
            _ => Err(crate::commands::not_implemented()),
        };
    }
    match basic::parse_args(args, &[], SUBCOMMANDS).map_err(|f| fail(ctx, f))? {
        Outcome::Help => help("configure"),
        Outcome::Run(_) => interactive(ctx),
    }
}

/// `ConfigureCommand._run_main`.
fn interactive(ctx: &Context) -> Result<ExitCode, CliError> {
    let config = ctx.session.config();
    let (scoped, _profile_exists) = match config.scoped_config() {
        Ok(s) => (s.clone(), true),
        Err(_) => (Section::new(), false),
    };
    if scoped.is_empty() {
        out(
            "\nTip: You can deliver temporary credentials to the AWS CLI using your AWS Console session by running the command 'aws login'.\n\n",
        );
    }
    let current = |name: &str| -> Option<String> {
        scoped.get(name).map(|v| match v {
            IniValue::Str(s) => s.clone(),
            IniValue::Nested(_) => crate::pyfmt::py_str(&Value::Map(Default::default())),
        })
    };
    let needs_token = |nv: &NewValues| match nv.get("aws_access_key_id") {
        Some(WriterValue::Str(k)) => k.starts_with("ASIA"),
        _ => false,
    };
    let has_current_token = current("aws_session_token").is_some_and(|t| !t.is_empty());
    let mut new_values = NewValues::new();
    for (name, text) in [
        ("aws_access_key_id", "AWS Access Key ID"),
        ("aws_secret_access_key", "AWS Secret Access Key"),
        ("aws_session_token", "AWS Session Token"),
        ("region", "Default region name"),
        ("output", "Default output format"),
    ] {
        if name == "aws_session_token" {
            let new_key = matches!(new_values.get("aws_access_key_id"), Some(WriterValue::Str(k)) if !k.is_empty());
            let should = if new_key && !needs_token(&new_values) {
                false
            } else {
                needs_token(&new_values) || has_current_token
            };
            if !should {
                continue;
            }
        }
        let current_value = current(name);
        let shown = if matches!(
            name,
            "aws_access_key_id" | "aws_secret_access_key" | "aws_session_token"
        ) {
            mask_value(current_value.as_deref())
        } else {
            current_value.clone().unwrap_or_else(|| "None".to_string())
        };
        let prompt_text = format!("{text} [{shown}]: ");
        let response = if name == "region"
            && std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
        {
            prompt::ptk_prompt(&prompt_text).map_err(prompt_error)?
        } else {
            prompt::input(&prompt_text).map_err(prompt_error)?
        };
        if !response.is_empty() && Some(&response) != current_value.as_ref() {
            new_values.insert(name.to_string(), WriterValue::Str(response));
        }
    }
    if new_values.contains_key("aws_access_key_id")
        && !needs_token(&new_values)
        && has_current_token
    {
        new_values.insert("aws_session_token".into(), WriterValue::None);
    }
    if new_values.is_empty() {
        return Ok(ExitCode::Success);
    }
    let profile = config.profile();
    let mut cred_values = NewValues::new();
    for k in [
        "aws_access_key_id",
        "aws_secret_access_key",
        "aws_session_token",
    ] {
        if let Some(v) = new_values.shift_remove(k) {
            cred_values.insert(k.to_string(), v);
        }
    }
    if !cred_values.is_empty() {
        update(
            profile.as_deref(),
            &cred_values,
            &credentials_file(config),
            true,
        )?;
    }
    let section = profile.as_deref().map(profile_to_section);
    update(section.as_deref(), &new_values, &config_file(config), false)?;
    Ok(ExitCode::Success)
}

const SUBSECTION_ARGS: [ArgSpec; 2] = [ArgSpec::option("sso-session"), ArgSpec::option("services")];

fn run_args(
    ctx: &Context,
    args: &[String],
    table: &[ArgSpec],
    name: &str,
) -> Result<Option<Args>, CliError> {
    match basic::parse_args(args, table, &[]).map_err(|f| fail(ctx, f))? {
        Outcome::Help => {
            help(name)?;
            Ok(None)
        }
        Outcome::Run(a) => Ok(Some(a)),
    }
}

/// `ConfigureGetCommand`.
fn get(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let mut table = vec![ArgSpec::positional("varname")];
    table.extend(SUBSECTION_ARGS);
    let Some(args) = run_args(ctx, args, &table, "get")? else {
        return Ok(ExitCode::Success);
    };
    basic::validate_mutually_exclusive(&args, &["sso_session", "services"])
        .map_err(|f| fail(ctx, f))?;
    let varname = args.str_arg("varname").unwrap_or_default();
    let config = ctx.session.config();
    let full = config.full_config();
    let subsection = if let Some(name) = args.str_arg("sso_session") {
        Some((&full.sso_sessions, name))
    } else {
        args.str_arg("services").map(|name| (&full.services, name))
    };
    let value: Option<IniValue> = if let Some((sections, name)) = subsection {
        let Some(section) = sections.get(&name) else {
            return Err(general(&format!(
                "The config sub-section ({name}) could not be found"
            )));
        };
        if varname.contains('.') {
            let parts: Vec<&str> = varname.split('.').collect();
            if parts.len() == 2 {
                match section.get(parts[0]) {
                    Some(IniValue::Nested(m)) => m.get(parts[1]).cloned().map(IniValue::Str),
                    _ => None,
                }
            } else {
                None
            }
        } else {
            section.get(&varname).cloned()
        }
    } else if !varname.contains('.') {
        let scoped = config.scoped_config().map_err(|e| ctx.session_error(&e))?;
        scoped.get(&varname).cloned()
    } else {
        dotted_config_value(config, &varname)?
    };
    match value {
        Some(IniValue::Str(s)) => {
            out(&format!("{s}\n"));
            Ok(ExitCode::Success)
        }
        Some(IniValue::Nested(_)) => {
            err(&format!(
                "varname ({varname}) must reference a value, not a section or sub-section."
            ));
            Ok(ExitCode::S3TransferFailed)
        }
        None => Ok(ExitCode::S3TransferFailed),
    }
}

/// `ConfigureGetCommand._get_dotted_config_value`.
fn dotted_config_value(config: &Config, varname: &str) -> Result<Option<IniValue>, CliError> {
    let full = config.full_config();
    let parts: Vec<&str> = varname.split('.').collect();
    let index = |i: usize| -> Result<&str, CliError> {
        parts
            .get(i)
            .copied()
            .ok_or_else(|| general("list index out of range"))
    };
    if parts.len() == 2 && is_predefined(parts[0]) {
        let (section, name) = (parts[0], parts[1]);
        let top = match section {
            "profiles" => None,
            other => full.other.get(other),
        };
        let value = top.and_then(|s| s.get(name).cloned()).or_else(|| {
            full.profiles
                .get(section)
                .and_then(|p| p.get(name).cloned())
        });
        return Ok(value);
    }
    let (profile_name, config_name, remaining): (String, &str, Vec<&str>) = if parts[0] == "profile"
    {
        (
            index(1)?.to_string(),
            index(2)?,
            parts[3.min(parts.len())..].to_vec(),
        )
    } else if parts[0] == "default" || full.profiles.contains_key(parts[0]) {
        (
            parts[0].to_string(),
            index(1)?,
            parts[2.min(parts.len())..].to_vec(),
        )
    } else {
        (
            config.profile().unwrap_or_else(|| "default".to_string()),
            parts[0],
            parts[1..].to_vec(),
        )
    };
    let mut value = full
        .profiles
        .get(&profile_name)
        .and_then(|p| p.get(config_name).cloned());
    if remaining.len() == 1 {
        value = match value {
            Some(IniValue::Nested(m)) => m.get(remaining[0]).cloned().map(IniValue::Str),
            _ => None,
        };
    }
    Ok(value)
}

/// `ConfigureSetCommand`.
fn set(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let mut value_arg = ArgSpec::positional("value");
    value_arg.no_paramfile = true;
    let mut table = vec![ArgSpec::positional("varname"), value_arg];
    table.extend(SUBSECTION_ARGS);
    let Some(args) = run_args(ctx, args, &table, "set")? else {
        return Ok(ExitCode::Success);
    };
    basic::validate_mutually_exclusive(&args, &["sso_session", "services"])
        .map_err(|f| fail(ctx, f))?;
    let config = ctx.session.config();
    let mut varname = args.str_arg("varname").unwrap_or_default();
    let raw_value = args.str_arg("value").unwrap_or_default();
    let mut value = WriterValue::Str(raw_value.clone());
    let subsection = [("sso-session", "sso_session"), ("services", "services")]
        .into_iter()
        .find_map(|(t, dest)| args.str_arg(dest).map(|n| (t, n)));
    if let Some((section_type, name)) = subsection {
        let valid = name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || "_-/.%@:+".contains(c));
        if !valid {
            return Err(fail(
                ctx,
                Fail::ParamValidation(format!(
                    "awsr: [ERROR]: Invalid value for --{section_type}."
                )),
            ));
        }
        if varname.contains('.') {
            let parts: Vec<String> = varname.split('.').map(str::to_string).collect();
            if parts.len() > 2 {
                return Err(fail(
                    ctx,
                    Fail::ParamValidation(
                        "Found more than two parts in the property to set. Deep nesting of properties is not supported."
                            .to_string(),
                    ),
                ));
            }
            varname = parts[0].clone();
            value = WriterValue::Nested([(parts[1].clone(), raw_value)].into_iter().collect());
        }
        let mut nv = NewValues::new();
        nv.insert(varname, value);
        update(
            Some(&get_section_header(section_type, &name)),
            &nv,
            &config_file(config),
            false,
        )?;
        return Ok(ExitCode::Success);
    }
    let session_profile = config.profile();
    let mut profile = "default".to_string();
    if !varname.contains('.') {
        if let Some(p) = &session_profile {
            profile = p.clone();
        }
    } else {
        let parts: Vec<String> = varname.split('.').map(str::to_string).collect();
        let index = |i: usize| -> Result<String, CliError> {
            parts
                .get(i)
                .cloned()
                .ok_or_else(|| general("list index out of range"))
        };
        if parts[0] == "default" || parts[0] == "profile" {
            let remaining: Vec<String> = if parts[0] == "default" {
                profile = "default".to_string();
                parts[1..].to_vec()
            } else {
                profile = index(1)?;
                parts[2..].to_vec()
            };
            varname = remaining
                .first()
                .cloned()
                .ok_or_else(|| general("list index out of range"))?;
            if remaining.len() == 2 {
                value =
                    WriterValue::Nested([(remaining[1].clone(), raw_value)].into_iter().collect());
            }
        } else if !is_predefined(&parts[0]) {
            if let Some(p) = &session_profile {
                profile = p.clone();
            }
            varname = parts[0].clone();
            if parts.len() == 2 {
                value = WriterValue::Nested([(parts[1].clone(), raw_value)].into_iter().collect());
            }
        } else if parts.len() == 2 {
            profile = parts[0].clone();
            varname = parts[1].clone();
        }
    }
    let (section, path, check) = if matches!(
        varname.as_str(),
        "aws_access_key_id" | "aws_secret_access_key" | "aws_session_token" | "aws_security_token"
    ) {
        (profile.clone(), credentials_file(config), true)
    } else if is_predefined(&profile) || profile == "default" {
        (profile.clone(), config_file(config), false)
    } else {
        (profile_to_section(&profile), config_file(config), false)
    };
    let mut nv = NewValues::new();
    nv.insert(varname, value);
    update(Some(&section), &nv, &path, check)?;
    Ok(ExitCode::Success)
}

/// `ConfigureListCommand`.
fn list(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    if run_args(ctx, args, &[], "list")?.is_none() {
        return Ok(ExitCode::Success);
    }
    let config = ctx.session.config();
    let row = |name: &str, value: &str, kind: &str, location: &str| {
        out(&format!(
            "{name:<10} : {} : {} : {location}\n",
            pad(value, 24),
            pad(kind, 16)
        ));
    };
    row("NAME", "VALUE", "TYPE", "LOCATION");
    if ctx.globals.profile.is_some() {
        row(
            "profile",
            &config.profile().unwrap_or_default(),
            "manual",
            "--profile",
        );
    } else {
        match ["AWS_PROFILE", "AWS_DEFAULT_PROFILE"]
            .iter()
            .find_map(|k| config.env(k))
        {
            Some(p) => row(
                "profile",
                p,
                "env",
                "['AWS_PROFILE', 'AWS_DEFAULT_PROFILE']",
            ),
            None => row("profile", NOT_SET, "None", "None"),
        }
    }
    let loaded = load_credentials(ctx).map_err(|e| ctx.session_error(&e))?;
    match loaded {
        Some(c) => {
            row(
                "access_key",
                &mask_value(Some(&c.credentials.access_key)),
                c.method,
                "",
            );
            row(
                "secret_key",
                &mask_value(Some(&c.credentials.secret_key)),
                c.method,
                "",
            );
        }
        None => {
            row("access_key", NOT_SET, "None", "None");
            row("secret_key", NOT_SET, "None", "None");
        }
    }
    if let Some(r) = ["AWS_REGION", "AWS_DEFAULT_REGION"]
        .iter()
        .find_map(|k| config.env(k))
    {
        row("region", r, "env", "['AWS_REGION', 'AWS_DEFAULT_REGION']");
    } else if let Some(r) = config
        .scoped_config()
        .map_err(|e| ctx.session_error(&e))?
        .get("region")
        .and_then(IniValue::as_str)
    {
        let location = config.get("config_file").ok().flatten().unwrap_or_default();
        row("region", r, "config-file", &location);
    } else {
        match config.region().map_err(|e| ctx.session_error(&e))? {
            Some(r) if !r.is_empty() => row("region", &r, "imds", ""),
            _ => row("region", NOT_SET, "None", "None"),
        }
    }
    Ok(ExitCode::Success)
}

/// `format(s, '<width')`.
fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - n))
    }
}

fn load_credentials(ctx: &Context) -> Result<Option<LoadedCredentials>, SessionError> {
    let region = ctx.session.config().region().ok().flatten();
    crate::session::useragent::load_credentials_in_command();
    crate::driver::block_on(
        &ctx.runtime,
        credentials::load_credentials(ctx.session.config(), region.as_deref(), None),
    )
}

/// `ListProfilesCommand`.
fn list_profiles(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    if run_args(ctx, args, &[], "list-profiles")?.is_none() {
        return Ok(ExitCode::Success);
    }
    let mut text = String::new();
    for name in ctx.session.config().full_config().profiles.keys() {
        text.push_str(name);
        text.push('\n');
    }
    out(&text);
    Ok(ExitCode::Success)
}

/// Python's `csv.reader([value])` first row (default dialect).
fn csv_row(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    if value.is_empty() {
        return out;
    }
    let mut field = String::new();
    let mut chars = value.chars().peekable();
    let mut in_quotes = false;
    let mut at_start = true;
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if at_start => {
                in_quotes = true;
                at_start = false;
            }
            ',' => {
                out.push(std::mem::take(&mut field));
                at_start = true;
            }
            c => {
                field.push(c);
                at_start = false;
            }
        }
    }
    out.push(field);
    out
}

/// `csv.writer(...).writerow(row)` stripped.
fn csv_line(row: &[String]) -> String {
    row.iter()
        .map(|f| {
            if f.contains([',', '"', '\r', '\n']) {
                format!("\"{}\"", f.replace('"', "\"\""))
            } else {
                f.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
        .trim()
        .to_string()
}

/// `ConfigureExportCredentialsCommand`.
fn export_credentials(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    const FORMATS: &[&str] = &[
        "process",
        "env",
        "env-no-export",
        "powershell",
        "windows-cmd",
        "fish",
    ];
    let mut format = ArgSpec::option("format");
    format.choices = Some(FORMATS);
    format.default = Some("process");
    let Some(args) = run_args(ctx, args, &[format], "export-credentials")? else {
        return Ok(ExitCode::Success);
    };
    let config = ctx.session.config();
    let profile = config.profile().unwrap_or_else(|| "default".to_string());
    let seen = csv_row(
        &credentials::child_env("_AWS_CLI_PROFILE_CHAIN").unwrap_or_else(|| {
            config
                .env("_AWS_CLI_PROFILE_CHAIN")
                .unwrap_or_default()
                .to_string()
        }),
    );
    if seen.len() >= 4 {
        return Err(configuration_error(
            ctx,
            &format!(
                "Maximum recursive credential process resolution reached (4).\nProfiles seen: {}",
                seen.join(" -> ")
            ),
        ));
    }
    if seen.contains(&profile) {
        let mut chain = seen.clone();
        chain.push(profile.clone());
        return Err(configuration_error(
            ctx,
            &format!(
                "Credential process resolution detected an infinite loop, profile cycle: {}\n",
                chain.join(" -> ")
            ),
        ));
    }
    let mut chain = seen;
    chain.push(profile);
    credentials::set_child_env("_AWS_CLI_PROFILE_CHAIN", &csv_line(&chain));
    let creds = match load_credentials(ctx) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Err(configuration_error(
                ctx,
                "Unable to retrieve credentials: no credentials found",
            ));
        }
        Err(e) if e.deferred => return Err(ctx.session_error(&e)),
        Err(e) => {
            return Err(configuration_error(
                ctx,
                &format!("Unable to retrieve credentials: {}\n", e.message.trim()),
            ));
        }
    };
    let expiry = creds.expiry.map(|e| e.isoformat());
    let c = &creds.credentials;
    let format = args.str_arg("format").unwrap_or_else(|| "process".into());
    if format == "process" {
        let q = |s: &str| crate::pyfmt::json_dumps_default(&Value::str(s));
        let mut fields = vec![
            "  \"Version\": 1".to_string(),
            format!("  \"AccessKeyId\": {}", q(&c.access_key)),
            format!("  \"SecretAccessKey\": {}", q(&c.secret_key)),
        ];
        if let Some(t) = &c.token {
            fields.push(format!("  \"SessionToken\": {}", q(t)));
        }
        if let Some(e) = &expiry {
            fields.push(format!("  \"Expiration\": {}", q(e)));
        }
        out(&format!("{{\n{}\n}}\n", fields.join(",\n")));
        return Ok(ExitCode::Success);
    }
    let line = |name: &str, value: &str| -> String {
        match format.as_str() {
            "env" => format!("export {name}={value}\n"),
            "env-no-export" => format!("{name}={value}\n"),
            "powershell" => format!("$Env:{name}=\"{value}\"\n"),
            "windows-cmd" => format!("set {name}={value}\n"),
            _ => format!("set -gx {name} \"{value}\"\n"),
        }
    };
    let mut text =
        line("AWS_ACCESS_KEY_ID", &c.access_key) + &line("AWS_SECRET_ACCESS_KEY", &c.secret_key);
    if let Some(t) = &c.token {
        text.push_str(&line("AWS_SESSION_TOKEN", t));
    }
    if let Some(e) = &expiry {
        text.push_str(&line("AWS_CREDENTIAL_EXPIRATION", e));
    }
    out(&text);
    Ok(ExitCode::Success)
}

/// `ConfigureImportCommand`.
fn import(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let mut csv = ArgSpec::option("csv");
    csv.required = true;
    let mut skip = ArgSpec::flag("skip-invalid");
    skip.dest = Some("skip_invalid");
    let mut prefix = ArgSpec::option("profile-prefix");
    prefix.dest = Some("profile_prefix");
    prefix.default = Some("");
    let Some(args) = run_args(ctx, args, &[csv, skip, prefix], "import")? else {
        return Ok(ExitCode::Success);
    };
    let contents = args.str_arg("csv").unwrap_or_default();
    let strict = !args.flag("skip_invalid");
    let profile_prefix = args.str_arg("profile_prefix").unwrap_or_default();
    if !contents.contains('\n')
        && std::path::Path::new(&contents).exists()
        && !contents.starts_with("file://")
    {
        return Err(general(
            "You may be passing a file to import without the 'file://' prefix. To import a CSV file, use --csv file://path/to/file.csv",
        ));
    }
    let config_path = credentials_file(ctx.session.config());
    warn_if_permissive(&config_path);
    let credentials = parse_credentials_csv(&contents, strict).map_err(|m| general(&m))?;
    for (name, akid, sak) in &credentials {
        let mut nv = NewValues::new();
        nv.insert("aws_access_key_id".into(), WriterValue::Str(akid.clone()));
        nv.insert(
            "aws_secret_access_key".into(),
            WriterValue::Str(sak.clone()),
        );
        update(
            Some(&format!("{profile_prefix}{name}")),
            &nv,
            &config_path,
            false,
        )?;
    }
    out(&format!(
        "Successfully imported {} profile(s)\n",
        credentials.len()
    ));
    Ok(ExitCode::Success)
}

/// `CSVCredentialParser.parse_credentials`.
fn parse_credentials_csv(
    contents: &str,
    strict: bool,
) -> Result<Vec<(String, String, String)>, String> {
    if contents.trim().is_empty() {
        return Err("Provided CSV contains no contents".to_string());
    }
    let format_header = |h: &str| {
        h.strip_prefix('\u{feff}')
            .unwrap_or(h)
            .to_lowercase()
            .trim()
            .to_string()
    };
    let lines = py_splitlines(contents);
    let headers: Vec<String> = lines[0].split(',').map(format_header).collect();
    let mut indices = Vec::new();
    for header in ["User Name", "Access Key ID", "Secret Access key"] {
        let formatted = format_header(header);
        let Some(i) = headers.iter().position(|h| *h == formatted) else {
            return Err(format!("Expected header \"{header}\" not found"));
        };
        indices.push((header, i));
    }
    let mut out = Vec::new();
    for (count, row) in lines[1..].iter().enumerate() {
        let cols: Vec<&str> = row.split(',').collect();
        let mut item = Vec::new();
        let mut error = None;
        for (header, index) in &indices {
            let value = cols.get(*index).map(|c| c.trim()).unwrap_or("");
            if value.is_empty() {
                error = Some(format!("Row missing value for header \"{header}\""));
                break;
            }
            item.push(value.to_string());
        }
        if let Some(e) = error {
            if !strict {
                continue;
            }
            return Err(format!("Failed to parse entry #{}: {e}", count + 1));
        }
        out.push((item[0].clone(), item[1].clone(), item[2].clone()));
    }
    Ok(out)
}

/// `str.splitlines()`.
fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes: Vec<(usize, char)> = s.char_indices().collect();
    let mut i = 0;
    while i < bytes.len() {
        let (pos, c) = bytes[i];
        if matches!(
            c,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            out.push(&s[start..pos]);
            let mut next = pos + c.len_utf8();
            if c == '\r' && bytes.get(i + 1).is_some_and(|(_, n)| *n == '\n') {
                i += 1;
                next += 1;
            }
            start = next;
        }
        i += 1;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    if out.is_empty() {
        out.push("");
    }
    out
}

/// `AddModelCommand`.
fn add_model(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let mut model_arg = ArgSpec::option("service-model");
    model_arg.required = true;
    let Some(args) = run_args(
        ctx,
        args,
        &[model_arg, ArgSpec::option("service-name")],
        "add-model",
    )?
    else {
        return Ok(ExitCode::Success);
    };
    let text = args.str_arg("service_model").unwrap_or_default();
    let definition = crate::cli::pyjson::loads(&text).map_err(|e| general(&e))?;
    let metadata = definition.get("metadata");
    let meta = |k: &str| {
        metadata
            .and_then(|m| m.get(k))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let service_name = match args.str_arg("service_name") {
        Some(n) => n,
        None => {
            let prefix = meta("endpointPrefix").ok_or_else(|| general("'endpointPrefix'"))?;
            if crate::model::has_service(&prefix) {
                prefix
            } else {
                let mut found = None;
                for name in crate::model::service_names() {
                    if let Ok(m) = crate::model::ServiceModel::load(name)
                        && m.endpoint_prefix() == prefix
                    {
                        found = Some(name.to_string());
                    }
                }
                found.unwrap_or(prefix)
            }
        }
    };
    let api_version = meta("apiVersion").ok_or_else(|| general("'apiVersion'"))?;
    let version = match definition.get("version") {
        Some(Value::Str(s)) => s.parse::<f64>().map_err(|_| {
            general(&format!(
                "could not convert string to float: {}",
                crate::pyfmt::str_repr(s)
            ))
        })?,
        Some(v) => crate::pyfmt::py_str(v).parse::<f64>().unwrap_or(2.0),
        None => 2.0,
    };
    let home = crate::session::cache::home_path(ctx.session.config(), &[".aws", "models"]);
    let dir = home.join(&service_name).join(&api_version);
    std::fs::create_dir_all(&dir).map_err(|e| general(&e.to_string()))?;
    std::fs::write(
        dir.join(format!("service-{}.json", version.trunc() as i64)),
        text.as_bytes(),
    )
    .map_err(|e| general(&e.to_string()))?;
    Ok(ExitCode::Success)
}

/// `ConfigureMFALoginCommand`.
fn mfa_login(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let mut duration = ArgSpec::option("duration-seconds");
    duration.cli_type = CliType::Integer;
    duration.default = Some("43200");
    let Some(args) = run_args(
        ctx,
        args,
        &[
            ArgSpec::option("update-profile"),
            duration,
            ArgSpec::option("serial-number"),
        ],
        "mfa-login",
    )?
    else {
        return Ok(ExitCode::Success);
    };
    let duration_seconds = args
        .get("duration_seconds")
        .and_then(basic::ArgValue::as_int)
        .unwrap_or(43200);
    let serial_arg = args.str_arg("serial_number").filter(|s| !s.is_empty());
    let update_profile = args.str_arg("update_profile").filter(|s| !s.is_empty());
    let config = ctx.session.config();
    let write_error = |m: &str| err(&render_general_error(m));
    let credential_value = |text: &str| -> Result<Option<String>, CliError> {
        let r = prompt::input(&format!("{text}: ")).map_err(prompt_error)?;
        Ok((!r.is_empty()).then_some(r))
    };
    let target_profile = |serial: &str| -> Result<String, CliError> {
        if let Some(p) = &update_profile {
            return Ok(p.clone());
        }
        let default = if serial.starts_with("arn:aws:iam::") {
            let parts: Vec<&str> = serial.split(':').collect();
            let device = parts
                .get(5)
                .and_then(|p| p.rsplit('/').next())
                .unwrap_or_default();
            format!("{}-{device}", parts.get(4).copied().unwrap_or_default())
        } else {
            format!("session-{serial}")
        };
        let r = prompt::input(&format!("Profile to update [{default}]: ")).map_err(prompt_error)?;
        Ok(if r.is_empty() { default } else { r })
    };
    let session_creds = load_credentials(ctx).map_err(|e| ctx.session_error(&e))?;
    let (sts_creds, serial, token, profile) = match session_creds {
        Some(c) => {
            let scoped = config.scoped_config().map_err(|e| ctx.session_error(&e))?;
            let serial = match serial_arg.or_else(|| {
                scoped
                    .get("mfa_serial")
                    .and_then(IniValue::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            }) {
                Some(s) => s,
                None => match credential_value("MFA serial number or ARN")? {
                    Some(s) => s,
                    None => {
                        write_error("MFA serial number or MFA device ARN is required");
                        return Ok(ExitCode::S3TransferFailed);
                    }
                },
            };
            let Some(token) = credential_value("MFA token code")? else {
                write_error("MFA token code is required");
                return Ok(ExitCode::S3TransferFailed);
            };
            let profile = target_profile(&serial)?;
            (c.credentials, serial, token, profile)
        }
        None => {
            out("Please provide your AWS credentials:\n");
            let mut values = std::collections::HashMap::new();
            for (name, text) in [
                ("aws_access_key_id", "AWS Access Key ID"),
                ("aws_secret_access_key", "AWS Secret Access Key"),
                ("mfa_serial", "MFA serial number or ARN"),
                ("mfa_token", "MFA token code"),
            ] {
                if name == "mfa_serial"
                    && let Some(s) = &serial_arg
                {
                    values.insert(name, s.clone());
                    continue;
                }
                match credential_value(text)? {
                    Some(v) if v != "None" => {
                        values.insert(name, v);
                    }
                    _ => {
                        write_error(&format!("{text} is required"));
                        return Ok(ExitCode::S3TransferFailed);
                    }
                }
            }
            let profile = target_profile(&values["mfa_serial"])?;
            (
                crate::session::sign::Credentials {
                    access_key: values["aws_access_key_id"].clone(),
                    secret_key: values["aws_secret_access_key"].clone(),
                    token: None,
                },
                values["mfa_serial"].clone(),
                values["mfa_token"].clone(),
                profile,
            )
        }
    };
    let region = config.region().ok().flatten();
    let client =
        crate::session::client::ApiClient::new(config, "sts", region, None, Some(sts_creds))
            .map_err(|e| ctx.session_error(&e))?;
    let mut params = crate::value::Map::new();
    params.insert("DurationSeconds".into(), Value::int(duration_seconds));
    params.insert("SerialNumber".into(), Value::str(&serial));
    params.insert("TokenCode".into(), Value::str(&token));
    let response = crate::driver::block_on(
        &ctx.runtime,
        client.call("GetSessionToken", &Value::Map(params), None),
    );
    let response = match response {
        Ok(r) => r,
        Err(crate::session::client::ApiError::Service(f)) => {
            write_error(&format!("An error occurred: {}", f.client_error_message()));
            return Ok(ExitCode::S3TransferFailed);
        }
        Err(crate::session::client::ApiError::Session(e)) => return Err(ctx.session_error(&e)),
    };
    let creds = response.get("Credentials").cloned().unwrap_or_default();
    let s = |k: &str| creds.get(k).map(crate::pyfmt::py_str).unwrap_or_default();
    let mut nv = NewValues::new();
    nv.insert(
        "aws_access_key_id".into(),
        WriterValue::Str(s("AccessKeyId")),
    );
    nv.insert(
        "aws_secret_access_key".into(),
        WriterValue::Str(s("SecretAccessKey")),
    );
    nv.insert(
        "aws_session_token".into(),
        WriterValue::Str(s("SessionToken")),
    );
    update(Some(&profile), &nv, &credentials_file(config), true)?;
    out(&format!(
        "Temporary credentials written to profile '{profile}'\nCredentials will expire at {}\nTo use these credentials, specify --profile {profile} when running AWS CLI commands\n",
        s("Expiration")
    ));
    Ok(ExitCode::Success)
}
