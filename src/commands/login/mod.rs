//! `aws login` and `aws signup` (`awscli/customizations/login/login.py`,
//! `signup.py`, `utils.py`): console credentials through AWS Sign-In with
//! PKCE and DPoP, cached in the login token cache.

use std::io::IsTerminal;

use base64::Engine;

use crate::commands::configure::basic::{self, ArgSpec, ArgsExt, CliType, Outcome};
use crate::commands::configure::writer::{NewValues, WriterValue};
use crate::commands::configure::{fail, general, out, prompt_error, writer_error};
use crate::commands::sso::flow::{self, AuthCodeFetcher, PendingHandler};
use crate::config::ini::IniValue;
use crate::driver::{Context, block_on};
use crate::error::{CliError, ExitCode};
use crate::session::cache;
use crate::session::client::{ApiClient, ApiError, percent_encode_sequence};
use crate::session::error::{ErrorKind, SessionError};
use crate::session::login_token::{self, DpopKey};
use crate::session::{prompt, pydatetime};
use crate::value::{Map, Value};

const SAME_DEVICE_CLIENT: &str = "arn:aws:signin:::devtools/same-device";
const CROSS_DEVICE_CLIENT: &str = "arn:aws:signin:::devtools/cross-device";

fn code_error(msg: &str) -> SessionError {
    SessionError::new(
        ErrorKind::General,
        format!("Error loading or redeeming a login authorization code: {msg} "),
    )
}

/// `SignupCommand`'s `DEFAULT_SIGNUP_REGION`.
const DEFAULT_SIGNUP_REGION: &str = "us-east-1";

/// `LoginCommand` and its subclass `SignupCommand`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Login,
    Signup,
}

impl Command {
    /// `NAME`.
    fn name(self) -> &'static str {
        match self {
            Command::Login => "login",
            Command::Signup => "signup",
        }
    }

    /// `AUTHORIZE_PROMPT`.
    fn authorize_prompt(self) -> Option<&'static str> {
        match self {
            Command::Login => None,
            Command::Signup => Some("create"),
        }
    }
}

/// `aws login`.
pub(crate) fn run(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    run_command(ctx, args, Command::Login)
}

/// `aws signup` (`SignupCommand`).
pub(crate) fn signup(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    run_command(ctx, args, Command::Signup)
}

/// `LoginCommand._run_main` of `command`.
fn run_command(ctx: &Context, args: &[String], command: Command) -> Result<ExitCode, CliError> {
    let mut port = ArgSpec::option("redirect-port");
    port.cli_type = CliType::Integer;
    let table = [ArgSpec::flag("remote"), port];
    let args = match basic::parse_args(args, &table, &[]).map_err(|f| fail(ctx, f))? {
        Outcome::Help => {
            out(&format!("{}\n", command.name()));
            return Ok(ExitCode::Success);
        }
        Outcome::Run(a) => a,
    };
    let redirect_port = crate::commands::sso::validate_redirect_port(ctx, &args)?;
    let config = ctx.session.config();
    let mut prompted_for_region = false;
    let region = match ctx.globals.region.clone().filter(|r| !r.is_empty()) {
        Some(r) => r,
        None if command == Command::Signup => DEFAULT_SIGNUP_REGION.to_string(),
        None => match config.region().ok().flatten().filter(|r| !r.is_empty()) {
            Some(r) => r,
            None => {
                prompted_for_region = true;
                out(
                    "No AWS region has been configured. The AWS region is the geographic location of your AWS resources.\n\nIf you have used AWS before and already have resources in your account, specify which region they were created in. If you have not created resources in your account before, you can pick the region closest to you: https://docs.aws.amazon.com/global-infrastructure/latest/regions/aws-regions.html.\n\nYou are able to change the region in the CLI at any time with the command \"aws configure set region NEW_REGION\".\n",
                );
                let r = prompt::ptk_prompt("AWS Region [us-east-1]: ").map_err(prompt_error)?;
                if r.is_empty() {
                    "us-east-1".to_string()
                } else {
                    r
                }
            }
        },
    };
    let profile_name = config.profile().unwrap_or_else(|| "default".to_string());
    let remote = args.flag("remote");
    let full = config.full_config();
    let is_new_profile = !full.profiles.contains_key(&profile_name);
    let profile = full
        .profiles
        .get(&profile_name)
        .cloned()
        .unwrap_or_default();
    let style = if profile.contains_key("web_identity_token_file") {
        Some("Web Identity")
    } else if profile.contains_key("sso_role_name") || profile.contains_key("sso_account_id") {
        Some("SSO")
    } else if profile.contains_key("aws_access_key_id") {
        Some("Access Key")
    } else if profile.contains_key("role_arn") {
        Some("Assume Role")
    } else if profile.contains_key("credential_process") {
        Some("Credential Process")
    } else {
        None
    };
    if let Some(style) = style {
        return Err(crate::commands::configure::configuration_error(
            ctx,
            &format!(
                "Profile '{profile_name}' is already configured with {style} credentials.\n\nYou may run 'aws {} --profile new-profile-name' to create a new profile with the specified name. Otherwise you must first manually remove the existing credentials from '{profile_name}'.\n",
                command.name()
            ),
        ));
    }
    let with_profile;
    let config = if is_new_profile {
        with_profile = config.with_profile(&profile_name);
        &with_profile
    } else {
        config
    };
    let client = ApiClient::new(
        config,
        "signin",
        Some(region.clone()),
        ctx.globals.endpoint_url.clone(),
        None,
    )
    .map_err(|e| ctx.session_error(&e))?;
    let key = DpopKey::generate();
    let auth_fetcher = if remote {
        None
    } else {
        Some(AuthCodeFetcher::new(redirect_port).map_err(|e| ctx.session_error(&e))?)
    };
    crate::session::useragent::register_command(&[if remote {
        crate::session::useragent::Feature::LoginCrossDevice
    } else {
        crate::session::useragent::Feature::LoginSameDevice
    }]);
    let mut probe_input = Map::new();
    probe_input.insert("clientId".into(), Value::str(" "));
    probe_input.insert("grantType".into(), Value::str(" "));
    let mut probe = Map::new();
    probe.insert("tokenInput".into(), Value::Map(probe_input));
    let base = flow::base_uri(
        &client
            .request_url("CreateOAuth2Token", &Value::Map(probe))
            .map_err(|e| ctx.session_error(&e.into_session_error()))?,
    );
    let state = uuid::Uuid::new_v4().to_string();
    let (verifier, challenge) = flow::pkce();
    let (client_id, redirect_uri) = match &auth_fetcher {
        Some(f) => (SAME_DEVICE_CLIENT, f.redirect_uri_with_port()),
        None => (
            CROSS_DEVICE_CLIENT,
            format!("{base}/v1/sessions/confirmation"),
        ),
    };
    let mut query_params = vec![
        ("response_type", "code".to_string()),
        ("client_id", client_id.to_string()),
        ("state", state.clone()),
        ("code_challenge_method", "SHA-256".to_string()),
        ("scope", "openid".to_string()),
        ("redirect_uri", redirect_uri.clone()),
    ];
    if let Some(prompt) = command.authorize_prompt() {
        query_params.push(("prompt", prompt.to_string()));
    }
    let query = percent_encode_sequence(&query_params);
    let uri = format!(
        "{base}/v1/authorize?{query}&code_challenge={}",
        &challenge[..challenge.len() - 1]
    );
    let (code, got_state) = match &auth_fetcher {
        Some(fetcher) => {
            PendingHandler::OpenBrowser.call(config, None, &uri, &uri, Some("--remote"));
            fetcher
                .get_auth_code_and_state(&state)
                .map_err(|e| ctx.session_error(&e))?
        }
        None => {
            PendingHandler::PrintOnly.call(config, None, &uri, &uri, Some("--remote"));
            let code = prompt::input("\nEnter the authorization code displayed in your browser: ")
                .map_err(prompt_error)?;
            parse_verification_code(&code).map_err(|m| general(&m))?
        }
    };
    let Some(code) = code else {
        return Err(ctx.session_error(&code_error("Failed to retrieve an authorization code.")));
    };
    let got_state = got_state.unwrap_or_default();
    if got_state != state {
        return Err(ctx.session_error(&code_error(&format!(
            "State parameter {got_state} does not match expected value {state}."
        ))));
    }
    let mut body = Map::new();
    body.insert("clientId".into(), Value::str(client_id));
    body.insert("grantType".into(), Value::str("authorization_code"));
    body.insert("code".into(), Value::str(&code));
    body.insert("codeVerifier".into(), Value::str(&verifier));
    body.insert("redirectUri".into(), Value::str(&redirect_uri));
    let mut params = Map::new();
    params.insert("tokenInput".into(), Value::Map(body));
    let dpop = |url: &str| vec![("DPoP".to_string(), key.dpop_header(url))];
    let response = match block_on(
        &ctx.runtime,
        client.call("CreateOAuth2Token", &Value::Map(params), Some(&dpop)),
    ) {
        Ok(r) => r,
        Err(ApiError::Service(f)) if f.code == "AccessDeniedException" => {
            let error_type = f.parsed.get("error").and_then(Value::as_str).unwrap_or("");
            let e = match error_type {
                "AUTHCODE_EXPIRED" => code_error(
                    "Unable to complete the login process due to an expired authorization code. Please reauthenticate using 'aws login'.",
                ),
                "INSUFFICIENT_PERMISSIONS" => SessionError::new(
                    ErrorKind::General,
                    "Unable to create or refresh login credentials due to insufficient permissions. You may be missing permission for the 'signin:CreateOAuth2Token' action.",
                ),
                _ => ApiError::Service(f).into_session_error(),
            };
            return Err(ctx.session_error(&e));
        }
        Err(e) => return Err(ctx.session_error(&e.into_session_error())),
    };
    let Some(output) = response.get("tokenOutput") else {
        return Err(ctx.session_error(&code_error("Failed to retrieve an access token.")));
    };
    let expires_in = output.get("expiresIn").and_then(Value::as_i64).unwrap_or(0);
    let id_token = output
        .get("idToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let session_id = login_session_from_id_token(&id_token).map_err(|m| general(&m))?;
    let parts: Vec<&str> = session_id.splitn(6, ':').collect();
    if parts.len() != 6 || parts[0] != "arn" {
        return Err(general(&format!(
            "Provided ARN: {session_id} must be of the format: arn:partition:service:region:account:resource"
        )));
    }
    let account_id = parts[4].to_string();
    let at = output.get("accessToken").cloned().unwrap_or_default();
    let mut access = Map::new();
    for k in ["accessKeyId", "secretAccessKey", "sessionToken"] {
        access.insert(k.into(), at.get(k).cloned().unwrap_or(Value::Null));
    }
    access.insert("accountId".into(), Value::str(account_id));
    access.insert(
        "expiresAt".into(),
        Value::str(pydatetime::utc_after(expires_in)),
    );
    let mut token = Map::new();
    token.insert("accessToken".into(), Value::Map(access));
    token.insert(
        "tokenType".into(),
        output.get("tokenType").cloned().unwrap_or(Value::Null),
    );
    token.insert("clientId".into(), Value::str(client_id));
    token.insert(
        "refreshToken".into(),
        output.get("refreshToken").cloned().unwrap_or(Value::Null),
    );
    token.insert("idToken".into(), Value::str(&id_token));
    token.insert("dpopKey".into(), Value::str(key.to_pem()));
    if let Some(existing) = profile.get("login_session").and_then(IniValue::as_str)
        && existing != session_id
    {
        let prompt_text = format!(
            "\nProfile {profile_name} is already configured to use session {existing}. Do you want to overwrite it to use {session_id} instead? (y/n): "
        );
        if !yes_no_choice(&prompt_text).map_err(prompt_error)? {
            return Ok(ExitCode::Success);
        }
    }
    if !has_endpoint_override(ctx) {
        token.insert("signInRegion".into(), Value::str(&region));
    }
    login_token::save_token(&cache::login_cache(config), &session_id, &Value::Map(token));
    let section = if profile_name == "default" {
        "default".to_string()
    } else {
        format!("profile {profile_name}")
    };
    let home_region = home_region(&id_token);
    let existing_region = profile
        .get("region")
        .and_then(IniValue::as_str)
        .map(str::to_string);
    let profile_region = match command {
        Command::Login => resolve_profile_region(
            &profile_name,
            &region,
            prompted_for_region,
            existing_region.as_deref(),
            home_region.as_deref(),
        ),
        Command::Signup => signup_profile_region(
            &profile_name,
            existing_region.as_deref(),
            home_region.as_deref(),
        ),
    };
    let mut nv = NewValues::new();
    nv.insert("login_session".into(), WriterValue::Str(session_id.clone()));
    if let Some(r) = profile_region.filter(|r| !r.is_empty()) {
        nv.insert("region".into(), WriterValue::Str(r));
    }
    let path = crate::cli::paramfile::expanduser(
        &config.get("config_file").ok().flatten().unwrap_or_default(),
    );
    crate::commands::configure::writer::update_config(Some(&section), &nv, &path, false)
        .map_err(writer_error)?;
    out(&format!(
        "\nUpdated profile {profile_name} to use {session_id} credentials.\n"
    ));
    if profile_name != "default" {
        out(&format!(
            "Use \"--profile {profile_name}\" to use the new credentials, such as \"aws sts get-caller-identity --profile {profile_name}\"\n"
        ));
    }
    Ok(ExitCode::Success)
}

/// `_resolve_endpoint_override`.
fn has_endpoint_override(ctx: &Context) -> bool {
    if ctx.globals.endpoint_url.is_some() {
        return true;
    }
    let config = ctx.session.config();
    if config
        .get_bool("ignore_configured_endpoint_urls")
        .ok()
        .flatten()
        == Some(true)
    {
        return false;
    }
    match config.configured_endpoint_url("signin") {
        Ok(u) => u.is_some(),
        Err(_) => {
            let id = crate::config::snake_case_service_id("signin").to_uppercase();
            config
                .env(&format!("AWS_ENDPOINT_URL_{id}"))
                .or_else(|| config.env("AWS_ENDPOINT_URL"))
                .is_some_and(|u| !u.is_empty())
        }
    }
}

/// `is_stdin_a_tty`.
fn is_stdin_a_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// `yes_no_choice`.
fn yes_no_choice(prompt_text: &str) -> Result<bool, prompt::PromptError> {
    loop {
        let r = prompt::input(prompt_text)?;
        match r.to_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => out("Invalid response. Please enter \"y\" or \"n\"\n"),
        }
    }
}

/// `LoginCommand._get_home_region`: Sign-In only includes it for accounts
/// that have a home region.
fn home_region(id_token: &str) -> Option<String> {
    decode_id_token_payload(id_token)
        .ok()?
        .get("aws_home_region")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `LoginCommand._resolve_profile_region`: the region to save to the
/// profile, if any.
fn resolve_profile_region(
    profile_name: &str,
    region: &str,
    prompted_for_region: bool,
    existing_region: Option<&str>,
    home_region: Option<&str>,
) -> Option<String> {
    if prompted_for_region {
        if should_use_home_region(profile_name, region, home_region) {
            return home_region.map(str::to_string);
        }
        return Some(region.to_string());
    }
    if let Some(existing) = existing_region.filter(|r| !r.is_empty()) {
        if should_use_home_region(profile_name, existing, home_region) {
            return home_region.map(str::to_string);
        }
        return None;
    }
    if !is_stdin_a_tty() {
        return None;
    }
    prompt_for_profile_region(
        profile_name,
        home_region.filter(|h| !h.is_empty()).unwrap_or(region),
    )
}

/// `LoginCommand._should_use_home_region`.
fn should_use_home_region(profile_name: &str, region: &str, home_region: Option<&str>) -> bool {
    let Some(home) = home_region.filter(|h| !h.is_empty() && *h != region) else {
        return false;
    };
    if !is_stdin_a_tty() {
        return false;
    }
    let question = format!(
        "\nProfile {profile_name} is configured for region {region}, but the home region for this account is {home}. Regional services outside the home region are denied. Do you want to set the region to {home}? (y/n): "
    );
    if !yes_no_choice(&question).unwrap_or(false) {
        return false;
    }
    print_home_region_set(home);
    true
}

/// `LoginCommand._print_home_region_set`.
fn print_home_region_set(home_region: &str) {
    out(&format!(
        "\nSet region to {home_region}, the home region for this account.\n"
    ));
}

/// `LoginCommand._prompt_for_profile_region`.
fn prompt_for_profile_region(profile_name: &str, default_region: &str) -> Option<String> {
    out(&format!("\nNo region is set for profile {profile_name}.\n"));
    let r = prompt::ptk_prompt(&format!("AWS Region [{default_region}]: ")).ok()?;
    Some(if r.is_empty() {
        default_region.to_string()
    } else {
        r
    })
}

/// `SignupCommand._resolve_profile_region`.
fn signup_profile_region(
    profile_name: &str,
    existing_region: Option<&str>,
    home_region: Option<&str>,
) -> Option<String> {
    let existing_region = existing_region.filter(|r| !r.is_empty());
    let Some(home) = home_region.filter(|h| !h.is_empty()) else {
        if existing_region.is_none() {
            out(&format!(
                "\nCould not determine the home region for this account. To set the region for this profile, run \"aws configure set region REGION --profile {profile_name}\".\n"
            ));
        }
        return None;
    };
    if let Some(existing) = existing_region {
        if should_use_home_region(profile_name, existing, Some(home)) {
            return Some(home.to_string());
        }
        return None;
    }
    print_home_region_set(home);
    Some(home.to_string())
}

/// `decode_id_token_payload`.
fn decode_id_token_payload(id_token: &str) -> Result<Value, String> {
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() != 3 {
        return Err("Invalid JWT token - cannot extract payload".to_string());
    }
    let mut data = parts[1].to_string();
    while !data.len().is_multiple_of(4) {
        data.push('=');
    }
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(data.as_bytes())
        .map_err(|_| "Incorrect padding".to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    crate::cli::pyjson::loads(&text)
}

/// `_extract_login_session_from_id_token`.
fn login_session_from_id_token(id_token: &str) -> Result<String, String> {
    let payload = decode_id_token_payload(id_token)?;
    payload
        .get("sub")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "Invalid JWT token - missing sub".to_string())
}

/// `CrossDeviceLoginTokenFetcher.parse_verification_code`.
fn parse_verification_code(code: &str) -> Result<(Option<String>, Option<String>), String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(code.trim().as_bytes())
        .map_err(|_| "Failed to decode the verification code.".to_string())?;
    let text = String::from_utf8(bytes)
        .map_err(|_| "Failed to decode the verification code.".to_string())?;
    let mut code = None;
    let mut state = None;
    for (k, v) in flow::parse_qs(&text) {
        match k.as_str() {
            "code" => code = Some(v),
            "state" => state = Some(v),
            _ => {}
        }
    }
    if code.is_none() || state.is_none() {
        return Err(
            "Failed to retrieve an auth_code or state from the verification code".to_string(),
        );
    }
    Ok((code, state))
}
