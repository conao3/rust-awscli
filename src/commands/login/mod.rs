//! `aws login` (`awscli/customizations/login/login.py`, `utils.py`):
//! console credentials through AWS Sign-In with PKCE and DPoP, cached in
//! the login token cache.

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

/// `aws login` (`LoginCommand._run_main`).
pub(crate) fn run(ctx: &Context, args: &[String]) -> Result<ExitCode, CliError> {
    let mut port = ArgSpec::option("redirect-port");
    port.cli_type = CliType::Integer;
    let table = [ArgSpec::flag("remote"), port];
    let args = match basic::parse_args(args, &table, &[]).map_err(|f| fail(ctx, f))? {
        Outcome::Help => {
            out("login\n");
            return Ok(ExitCode::Success);
        }
        Outcome::Run(a) => a,
    };
    let redirect_port = crate::commands::sso::validate_redirect_port(ctx, &args)?;
    let config = ctx.session.config();
    let mut prompted_for_region = false;
    let region = match ctx.globals.region.clone().filter(|r| !r.is_empty()) {
        Some(r) => r,
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
                "Profile '{profile_name}' is already configured with {style} credentials.\n\nYou may run 'aws login --profile new-profile-name' to create a new profile with the specified name. Otherwise you must first manually remove the existing credentials from '{profile_name}'.\n"
            ),
        ));
    }
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
    let query = percent_encode_sequence(&[
        ("response_type", "code".to_string()),
        ("client_id", client_id.to_string()),
        ("state", state.clone()),
        ("code_challenge_method", "SHA-256".to_string()),
        ("scope", "openid".to_string()),
        ("redirect_uri", redirect_uri.clone()),
    ]);
    let uri = format!(
        "{base}/v1/authorize?{query}&code_challenge={}",
        &challenge[..challenge.len() - 1]
    );
    let (code, got_state) = match &auth_fetcher {
        Some(fetcher) => {
            PendingHandler::OpenBrowser.call(config, None, &uri, &uri, Some("--remote"));
            fetcher
                .get_auth_code_and_state()
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
        loop {
            let r = prompt::input(&prompt_text).map_err(prompt_error)?;
            match r.to_lowercase().as_str() {
                "y" | "yes" => break,
                "n" | "no" => return Ok(ExitCode::Success),
                _ => out("Invalid response. Please enter \"y\" or \"n\"\n"),
            }
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
    let mut nv = NewValues::new();
    nv.insert("login_session".into(), WriterValue::Str(session_id.clone()));
    if prompted_for_region {
        nv.insert("region".into(), WriterValue::Str(region.clone()));
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
    let _ = is_new_profile;
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

/// `_extract_login_session_from_id_token`.
fn login_session_from_id_token(id_token: &str) -> Result<String, String> {
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
    let payload = crate::cli::pyjson::loads(&text)?;
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
