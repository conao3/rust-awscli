//! `aws configure sso` and `aws configure sso-session`
//! (`awscli/customizations/configure/sso_commands.py`, `sso.py`).

use indexmap::IndexMap;

use super::basic::{self, ArgsExt, Outcome};
use super::writer::{NewValues, WriterValue};
use super::{fail, get_section_header, out, profile_to_section, prompt_error, writer_error};
use crate::commands::sso::flow::{self, PendingHandler, SsoLogin};
use crate::commands::sso::{self as sso_cmd, parse_scopes};
use crate::config::Config;
use crate::driver::{Context, block_on};
use crate::error::{CliError, ExitCode};
use crate::session::client::{ApiClient, ApiError};
use crate::session::prompt;
use crate::value::{Map, Value};

const DEFAULT_SSO_SCOPE: &str = "sso:account:access";

/// `PTKPrompt.get_value`.
fn get_value(
    current: Option<&str>,
    text: &str,
    prompt_fmt: Option<&str>,
) -> Result<Option<String>, CliError> {
    let prompt = match prompt_fmt {
        Some(f) => f.to_string(),
        None => format!("{text} [{}]: ", current.unwrap_or("None")),
    };
    let response = prompt::ptk_prompt(&prompt).map_err(prompt_error)?;
    Ok(if response.is_empty() {
        current.map(str::to_string)
    } else {
        Some(response)
    })
}

/// `SSOSessionConfigurationPrompter`.
struct SessionPrompter {
    sso_sessions: IndexMap<String, IndexMap<String, String>>,
    sso_session: Option<String>,
    /// `sso_session_config`; in the legacy flow this is the profile's new
    /// values (the same dict in Python).
    config: IndexMap<String, String>,
}

impl SessionPrompter {
    fn new(config: &Config) -> SessionPrompter {
        let sso_sessions = config
            .full_config()
            .sso_sessions
            .iter()
            .map(|(name, section)| {
                let values = section
                    .iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect();
                (name.clone(), values)
            })
            .collect();
        SessionPrompter {
            sso_sessions,
            sso_session: None,
            config: IndexMap::new(),
        }
    }

    fn set_sso_session(&mut self, value: Option<String>) {
        self.config = value
            .as_ref()
            .and_then(|v| self.sso_sessions.get(v))
            .cloned()
            .unwrap_or_default();
        self.sso_session = value;
    }

    fn prompt_for(
        &mut self,
        name: &str,
        text: &str,
        prompt_fmt: Option<&str>,
        current: Option<String>,
    ) -> Result<Option<String>, CliError> {
        let current = current.or_else(|| self.config.get(name).cloned());
        let value = get_value(current.as_deref(), text, prompt_fmt)?;
        if let Some(v) = value.as_ref().filter(|v| !v.is_empty()) {
            self.config.insert(name.to_string(), v.clone());
        }
        Ok(value)
    }

    fn prompt_for_sso_session(&mut self, required: bool) -> Result<Option<String>, CliError> {
        let fmt = if self.sso_session.is_none() {
            Some(if required {
                "SSO session name: "
            } else {
                "SSO session name (Recommended): "
            })
        } else {
            None
        };
        let current = self.sso_session.clone();
        let value = self.prompt_for("sso_session", "SSO session name", fmt, current)?;
        let value = value.filter(|v| !v.is_empty());
        self.set_sso_session(value.clone());
        Ok(value)
    }

    fn prompt_for_start_url(&mut self) -> Result<String, CliError> {
        Ok(self
            .prompt_for("sso_start_url", "SSO start URL", None, None)?
            .unwrap_or_default())
    }

    fn prompt_for_region(&mut self) -> Result<String, CliError> {
        Ok(self
            .prompt_for("sso_region", "SSO region", None, None)?
            .unwrap_or_default())
    }

    fn prompt_for_scopes(&mut self) -> Result<Vec<String>, CliError> {
        if !self.config.contains_key("sso_registration_scopes") {
            self.config
                .insert("sso_registration_scopes".into(), DEFAULT_SSO_SCOPE.into());
        }
        let raw = self
            .prompt_for(
                "sso_registration_scopes",
                "SSO registration scopes",
                None,
                None,
            )?
            .unwrap_or_default();
        Ok(parse_scopes(&raw))
    }
}

fn update_section(
    config: &Config,
    section: &str,
    values: &IndexMap<String, String>,
) -> Result<(), CliError> {
    let nv: NewValues = values
        .iter()
        .map(|(k, v)| (k.clone(), WriterValue::Str(v.clone())))
        .collect();
    let path = crate::cli::paramfile::expanduser(
        &config.get("config_file").ok().flatten().unwrap_or_default(),
    );
    super::writer::update_config(Some(section), &nv, &path, false).map_err(writer_error)
}

/// Resolve a start URL that is not AWS owned; `None` falls back to the
/// region prompt.
fn resolve_vanity(ctx: &Context, start_url: &str) -> Option<(String, String)> {
    let (_, host) = flow::url_host(start_url);
    let host = host?;
    if flow::is_aws_owned_domain(&host) {
        return None;
    }
    let resolved = block_on(
        &ctx.runtime,
        flow::resolve_start_url(ctx.session.config(), start_url, None),
    )
    .ok();
    if resolved.is_some() {
        crate::session::useragent::register_command(&[
            crate::session::useragent::Feature::SsoLoginVanityUrl,
        ]);
    }
    resolved
}

/// `aws configure sso-session`.
pub(crate) fn configure_sso_session(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    match basic::parse_args(args, &[], &[]).map_err(|f| fail(ctx, f))? {
        Outcome::Help => {
            out("sso-session\n");
            return Ok(ExitCode::Success);
        }
        Outcome::Run(_) => {}
    }
    let config = ctx.session.config();
    let mut prompter = SessionPrompter::new(config);
    prompter.prompt_for_sso_session(true)?;
    let start_url = prompter.prompt_for_start_url()?;
    match resolve_vanity(ctx, &start_url) {
        Some((_, region)) => {
            prompter.config.insert("sso_region".into(), region);
        }
        None => {
            prompter.prompt_for_region()?;
        }
    }
    prompter.prompt_for_scopes()?;
    let name = prompter.sso_session.clone().unwrap_or_default();
    update_section(
        config,
        &get_section_header("sso-session", &name),
        &prompter.config,
    )?;
    out(&format!(
        "\nCompleted configuring SSO session: {name}\nRun the following to login and refresh access token for this session:\n\naws sso login --sso-session {name}\n"
    ));
    Ok(ExitCode::Success)
}

/// `display_account`.
fn display_account(account: &Value) -> String {
    let s = |k: &str| account.get(k).and_then(Value::as_str);
    match (
        s("accountName"),
        s("emailAddress"),
        s("accountId").unwrap_or_default(),
    ) {
        (None, None, id) => id.to_string(),
        (Some(n), None, id) => format!("{n} ({id})"),
        (None, Some(e), id) => format!("{e} ({id})"),
        (Some(n), Some(e), id) => format!("{n}, {e} ({id})"),
    }
}

/// What ends the account and role selection.
enum StepError {
    Api(ApiError),
    Prompt(prompt::PromptError),
}

impl From<ApiError> for StepError {
    fn from(e: ApiError) -> StepError {
        StepError::Api(e)
    }
}

/// `select_menu(items, display_format=...)`.
fn select(items: &[Value], display: impl Fn(&Value) -> String) -> Result<&Value, StepError> {
    let shown: Vec<String> = items.iter().map(display).collect();
    let index = prompt::select_menu(&shown).map_err(StepError::Prompt)?;
    Ok(&items[index])
}

async fn paginate(
    client: &ApiClient<'_>,
    operation: &str,
    params: Map,
    result_key: &str,
) -> Result<Vec<Value>, ApiError> {
    let mut out = Vec::new();
    let mut next: Option<Value> = None;
    loop {
        let mut p = params.clone();
        if let Some(n) = &next {
            p.insert("nextToken".into(), n.clone());
        }
        let response = client.call(operation, &Value::Map(p), None).await?;
        if let Some(list) = response.get(result_key).and_then(Value::as_list) {
            out.extend(list.iter().cloned());
        }
        match response.get("nextToken") {
            Some(t) if t.as_str().is_some_and(|s| !s.is_empty()) => next = Some(t.clone()),
            _ => break,
        }
    }
    Ok(out)
}

/// `aws configure sso` (`ConfigureSSOCommand._run_main`).
pub(crate) fn configure_sso(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let args =
        match basic::parse_args(args, &sso_cmd::login_args(), &[]).map_err(|f| fail(ctx, f))? {
            Outcome::Help => {
                out("sso\n");
                return Ok(ExitCode::Success);
            }
            Outcome::Run(a) => a,
        };
    let config = ctx.session.config();
    let original_profile = config.profile();
    let profile_config: IndexMap<String, String> = config
        .scoped_config()
        .map(|s| {
            s.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let redirect_port = sso_cmd::validate_redirect_port(ctx, &args)?;
    let mut prompter = SessionPrompter::new(config);
    if let Some(s) = profile_config.get("sso_session") {
        prompter.set_sso_session(Some(s.clone()));
    }
    let handler = if args.flag("no_browser") {
        PendingHandler::PrintOnly
    } else {
        PendingHandler::OpenBrowser
    };
    let mut new_values: IndexMap<String, String> = IndexMap::new();
    let defaults_from_profile = |prompter: &mut SessionPrompter| {
        for k in ["sso_start_url", "sso_region"] {
            if let Some(v) = profile_config.get(k) {
                prompter.config.insert(k.into(), v.clone());
            }
        }
    };
    let sso_session = prompter.prompt_for_sso_session(false)?;
    let mut login = SsoLogin {
        sso_region: String::new(),
        start_url: String::new(),
        resolved_start_url: None,
        session_name: None,
        registration_scopes: None,
        force_refresh: false,
        use_device_code: args.flag("use_device_code"),
        redirect_port,
        handler,
    };
    let legacy = sso_session.is_none();
    match &sso_session {
        None => {
            out(
                "\x1b[1mWARNING: Configuring using legacy format (e.g. without an SSO session).\nConsider re-running \"configure sso\" command and providing a session name.\n\x1b[0m",
            );
            prompter.config = new_values.clone();
            defaults_from_profile(&mut prompter);
            let start_url = prompter.prompt_for_start_url()?;
            let (region, resolved) = match resolve_vanity(ctx, &start_url) {
                Some((resolved, region)) => {
                    prompter.config.insert("sso_region".into(), region.clone());
                    (region, Some(resolved))
                }
                None => (prompter.prompt_for_region()?, None),
            };
            new_values = prompter.config.clone();
            login.start_url = start_url;
            login.sso_region = region;
            login.resolved_start_url = resolved;
        }
        Some(name) => {
            new_values.insert("sso_session".into(), name.clone());
            if prompter.sso_sessions.contains_key(name) {
                let sso = sso_cmd::get_sso_config(ctx, Some(name.clone()))
                    .map_err(|e| ctx.session_error(&e))?;
                login.session_name = Some(name.clone());
                login.start_url = sso.start_url;
                login.sso_region = sso.region.unwrap_or_default();
                login.registration_scopes = sso.registration_scopes;
            } else {
                defaults_from_profile(&mut prompter);
                let start_url = prompter.prompt_for_start_url()?;
                let (region, resolved) = match resolve_vanity(ctx, &start_url) {
                    Some((resolved, region)) => {
                        prompter.config.insert("sso_region".into(), region.clone());
                        (region, Some(resolved))
                    }
                    None => (prompter.prompt_for_region()?, None),
                };
                let scopes = prompter.prompt_for_scopes()?;
                login.session_name = Some(name.clone());
                login.start_url = start_url;
                login.sso_region = region;
                login.registration_scopes = Some(scopes);
                login.force_refresh = true;
                login.resolved_start_url = resolved;
            }
        }
    }
    let token = block_on(&ctx.runtime, flow::do_sso_login(config, &login))
        .map_err(|e| ctx.session_error(&e))?;
    let access_token = token.get("accessToken").cloned().unwrap_or(Value::Null);
    let client = ApiClient::new(config, "sso", Some(login.sso_region.clone()), None, None)
        .map_err(|e| ctx.session_error(&e))?;
    let mut account_id: Option<String> = None;
    let mut role_name: Option<String> = None;
    let result: Result<(), StepError> = block_on(&ctx.runtime, async {
        let mut params = Map::new();
        params.insert("accessToken".into(), access_token.clone());
        let accounts = paginate(&client, "ListAccounts", params, "accountList").await?;
        if accounts.is_empty() {
            return Err(StepError::Api(ApiError::Session(
                crate::session::error::SessionError::new(
                    crate::session::error::ErrorKind::General,
                    "No AWS accounts are available to you.",
                ),
            )));
        }
        let id = if accounts.len() == 1 {
            let id = accounts[0]
                .get("accountId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            out(&format!("The only AWS account available to you is: {id}\n"));
            id
        } else {
            out(&format!(
                "There are {} AWS accounts available to you.\n",
                accounts.len()
            ));
            let mut sorted = accounts.clone();
            sorted.sort_by_key(|a| {
                let only_id = a.get("accountName").is_none() && a.get("emailAddress").is_none();
                let key = ["accountName", "emailAddress", "accountId"]
                    .iter()
                    .find_map(|k| a.get(k).and_then(Value::as_str))
                    .map(str::to_lowercase);
                (only_id, key)
            });
            select(&sorted, display_account)?
                .get("accountId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        out(&format!("Using the account ID {id}\n"));
        account_id = Some(id.clone());
        let mut params = Map::new();
        params.insert("accountId".into(), Value::str(&id));
        params.insert("accessToken".into(), access_token.clone());
        let roles = paginate(&client, "ListAccountRoles", params, "roleList").await?;
        if roles.is_empty() {
            return Err(StepError::Api(ApiError::Session(
                crate::session::error::SessionError::new(
                    crate::session::error::ErrorKind::General,
                    format!("No roles are available for the account {id}"),
                ),
            )));
        }
        let name = if roles.len() == 1 {
            let n = roles[0]
                .get("roleName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            out(&format!("The only role available to you is: {n}\n"));
            n
        } else {
            out(&format!(
                "There are {} roles available to you.\n",
                roles.len()
            ));
            let mut sorted = roles.clone();
            sorted.sort_by_key(|r| {
                r.get("roleName")
                    .and_then(Value::as_str)
                    .map(str::to_lowercase)
            });
            select(&sorted, |r| {
                r.get("roleName")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })?
            .get("roleName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
        };
        out(&format!("Using the role name \"{name}\"\n"));
        role_name = Some(name);
        Ok(())
    });
    match result {
        Ok(()) => {}
        Err(StepError::Api(ApiError::Service(f))) if f.code == "UnauthorizedException" => {
            out(
                "Unable to list AWS accounts and/or roles. Skipping configuring AWS credential provider for profile.\n",
            );
        }
        Err(StepError::Prompt(prompt::PromptError::Interrupted)) => {
            return Err(CliError::new(ExitCode::Interrupted, ""));
        }
        Err(StepError::Prompt(e)) => return Err(prompt_error(e)),
        Err(StepError::Api(e)) => return Err(ctx.session_error(&e.into_session_error())),
    }
    if let Some(id) = &account_id {
        new_values.insert("sso_account_id".into(), id.clone());
    }
    if let Some(r) = &role_name {
        new_values.insert("sso_role_name".into(), r.clone());
    }
    let configured = account_id.is_some() && role_name.is_some();
    if let Some(v) = get_value(
        profile_config.get("region").map(String::as_str),
        "Default client Region",
        None,
    )?
    .filter(|v| !v.is_empty())
    {
        new_values.insert("region".into(), v);
    }
    if let Some(v) = get_value(
        profile_config.get("output").map(String::as_str),
        "CLI default output format (json if not specified)",
        None,
    )?
    .filter(|v| !v.is_empty())
    {
        new_values.insert("output".into(), v);
    }
    let profile_name = match &original_profile {
        Some(p) => p.clone(),
        None => {
            let default = match (&account_id, &role_name) {
                (Some(a), Some(r)) => Some(format!("{r}-{a}")),
                _ => None,
            };
            get_value(default.as_deref(), "Profile name", None)?.unwrap_or_default()
        }
    };
    if !new_values.is_empty() {
        update_section(config, &profile_to_section(&profile_name), &new_values)?;
    }
    if let Some(name) = &prompter.sso_session
        && !legacy
    {
        update_section(
            config,
            &get_section_header("sso-session", name),
            &prompter.config,
        )?;
    }
    let msg = if configured {
        if profile_name.to_lowercase() == "default" {
            "The AWS CLI is now configured to use the default profile.\nRun the following command to verify your configuration:\n\naws sts get-caller-identity\n".to_string()
        } else {
            format!(
                "To use this profile, specify the profile name using --profile, as shown:\n\naws sts get-caller-identity --profile {profile_name}\n"
            )
        }
    } else {
        format!("Successfully configured SSO for profile: {profile_name}\n")
    };
    out(&msg);
    Ok(ExitCode::Success)
}
