//! `awscli/customizations/agentregistry/providers.py`: the identity
//! provider adapters (`CognitoProvider`, `GenericOidcProvider`), the one
//! sign-in of `list-skills` and `install-skill`, and the bearer token a
//! command sends.

use std::sync::LazyLock;

use regex::Regex;

use super::browserflow::run_browser_login;
use super::cache::{self, RefreshError, Saved, read_entry, save_token};
use super::constants::TOKEN_ENV_VAR;
use super::utils::{
    bare_registry_id, discover_registry, fetch_oidc_metadata, note_preset_token, printable, prompt,
    redact_logs_and_history, remember_signed_in_registry, requested_scopes, same_issuer,
    select_bearer_token, stdin_is_terminal, str_of, unique, url_origin, with_scopes, write_status,
};
use super::{Error, Result};
use crate::driver::Context;
use crate::value::{Map, Value};

/// `CognitoProvider._ISSUER`.
static COGNITO_ISSUER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\Ahttps://cognito-idp\.([a-z0-9-]+)\.amazonaws\.com(?:\.cn)?/([a-z0-9-]+_[A-Za-z0-9]+)\z",
    )
    .expect("regex")
});

enum Provider {
    Cognito { issuer: String },
    Generic { issuer: String },
}

/// `identify_provider(issuer)`.
fn identify_provider(issuer: &str) -> Result<Provider> {
    if COGNITO_ISSUER.is_match(issuer.trim_end_matches('/')) {
        return Ok(Provider::Cognito {
            issuer: issuer.to_string(),
        });
    }
    if issuer.starts_with("https://") {
        return Ok(Provider::Generic {
            issuer: issuer.to_string(),
        });
    }
    Err(Error::Jwt(format!(
        "Unsupported identity provider: {}",
        printable(issuer)
    )))
}

impl Provider {
    fn display_name(&self) -> &'static str {
        match self {
            Provider::Cognito { .. } => "Amazon Cognito",
            Provider::Generic { .. } => "generic OIDC provider",
        }
    }
}

/// The arguments of a sign-in.
pub(crate) struct SignIn<'a> {
    pub endpoint: &'a str,
    pub registry_id: &'a str,
    pub client_id: Option<String>,
    pub ca_bundle: Option<&'a str>,
    pub open_browser: bool,
}

/// `sign_in(session, endpoint, registry_id, ...)`.
fn sign_in(ctx: &Context, args: SignIn<'_>) -> Result<String> {
    let registry = discover_registry(args.endpoint, args.registry_id, args.ca_bundle)?;
    let provider = identify_provider(&registry.issuer)?;
    write_status(&format!(
        "\nRegistry {} trusts: {}",
        args.registry_id, registry.issuer
    ));
    write_status(&format!(
        "Detected identity provider: {}",
        provider.display_name()
    ));
    let client_id = match args.client_id.filter(|c| !c.is_empty()) {
        Some(c) => Some(c),
        None => previous_client_id(args.endpoint, args.registry_id, &registry.issuer),
    };
    match &provider {
        Provider::Cognito { issuer } => {
            let (_, pool_id) = pool(issuer);
            write_status(&format!("User pool: {pool_id}"));
            let client_id = client_id_or_prompt(client_id, "Cognito app client id")?;
            cognito_browser_login(
                ctx,
                issuer,
                args.endpoint,
                args.registry_id,
                &client_id,
                args.ca_bundle,
                &registry.scopes,
                args.open_browser,
            )
        }
        Provider::Generic { issuer } => {
            let metadata = fetch_oidc_metadata(issuer, args.ca_bundle)?;
            let token_endpoint = str_of(&metadata, "token_endpoint").unwrap_or_default();
            let authorize_endpoint =
                str_of(&metadata, "authorization_endpoint").unwrap_or_default();
            if authorize_endpoint.is_empty() || token_endpoint.is_empty() {
                return Err(Error::Jwt(format!(
                    "{issuer} does not advertise both an authorization_endpoint and a token_endpoint, so a sign-in link cannot be built."
                )));
            }
            let client_id = client_id_or_prompt(client_id, "OAuth client id")?;
            let scopes = with_scopes(&requested_scopes(&metadata), &registry.scopes);
            let payload = run_browser_login(
                ctx,
                authorize_endpoint,
                token_endpoint,
                &client_id,
                &scopes,
                args.ca_bundle,
                args.open_browser,
            )?;
            let token = str_of(&payload, "access_token")
                .unwrap_or_default()
                .to_string();
            save_token(
                args.endpoint,
                args.registry_id,
                &Saved {
                    token: &token,
                    expires_in: payload.get("expires_in").cloned(),
                    refresh_token: str_of(&payload, "refresh_token").map(str::to_string),
                    client_id: Some(client_id),
                    issuer: Some(issuer.clone()),
                    revocation_endpoint: revocation_endpoint(&metadata),
                    pool_region: None,
                    token_endpoint: Some(token_endpoint.to_string()),
                    scopes: granted_scopes(&payload, &scopes),
                },
            );
            Ok(token)
        }
    }
}

/// `use_bearer_token(...)`: the token this command sends, or `None` when
/// one is already in `AWS_BEARER_TOKEN_AGENT_REGISTRY`.
pub(crate) fn use_bearer_token(ctx: &Context, args: SignIn<'_>) -> Result<Option<String>> {
    if let Ok(preset) = std::env::var(TOKEN_ENV_VAR)
        && !preset.is_empty()
    {
        redact_logs_and_history(&[&preset]);
        note_preset_token(&["--client-id", "--no-browser"]);
        return Ok(None);
    }
    let registry_id = bare_registry_id(args.registry_id)?;
    let token = match cache::load_token(
        ctx,
        args.endpoint,
        &registry_id,
        args.client_id.as_deref(),
        args.ca_bundle,
    )? {
        Some(t) => t,
        None => sign_in(
            ctx,
            SignIn {
                registry_id: &registry_id,
                ..args
            },
        )?,
    };
    redact_logs_and_history(&[&token]);
    remember_signed_in_registry(&registry_id);
    crate::debuglog::debug(
        "awscli.customizations.agentregistry.providers",
        &format!("Exported {TOKEN_ENV_VAR}"),
    );
    Ok(Some(token))
}

/// `_previous_client_id(endpoint, registry_id, issuer)`.
fn previous_client_id(endpoint: &str, registry_id: &str, issuer: &str) -> Option<String> {
    let entry = read_entry(endpoint, registry_id)?;
    let client_id = str_of(&entry, "clientId").filter(|c| !c.is_empty())?;
    let previous = str_of(&entry, "issuer")?;
    if !same_issuer(previous, issuer) {
        return None;
    }
    write_status(&format!(
        "Using client id {} from the previous sign-in.",
        printable(client_id)
    ));
    Some(client_id.to_string())
}

/// `_client_id(client_id, label)`.
fn client_id_or_prompt(client_id: Option<String>, label: &str) -> Result<String> {
    if let Some(c) = client_id.filter(|c| !c.is_empty()) {
        return Ok(c);
    }
    if !stdin_is_terminal() {
        return Err(Error::Jwt(
            "An OAuth client id is required, and standard input is not a terminal to prompt for it. Pass --client-id with the id the registry owner provides.".to_string(),
        ));
    }
    let answer = prompt(&format!("{label} (from the registry owner): "))?;
    if answer.is_empty() {
        return Err(Error::Jwt(
            "An OAuth client id is required. Protected Resource Metadata does not publish it, so the registry owner must provide it.".to_string(),
        ));
    }
    Ok(answer)
}

/// `_granted_scopes(payload, requested)`.
fn granted_scopes(payload: &Value, requested: &[String]) -> Vec<String> {
    let granted = str_of(payload, "scope").unwrap_or_default();
    unique(
        requested
            .iter()
            .cloned()
            .chain(granted.split_whitespace().map(str::to_string)),
    )
}

/// `_revocation_endpoint(metadata)`.
fn revocation_endpoint(metadata: &Value) -> Option<String> {
    str_of(metadata, "revocation_endpoint")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The region and pool id of a Cognito issuer.
fn pool(issuer: &str) -> (String, String) {
    let c = COGNITO_ISSUER
        .captures(issuer.trim_end_matches('/'))
        .expect("a Cognito issuer");
    (c[1].to_string(), c[2].to_string())
}

/// `CognitoProvider._browser_login(...)`.
#[allow(clippy::too_many_arguments)]
fn cognito_browser_login(
    ctx: &Context,
    issuer: &str,
    endpoint: &str,
    registry_id: &str,
    client_id: &str,
    ca_bundle: Option<&str>,
    extra_scopes: &[String],
    open_browser: bool,
) -> Result<String> {
    let (pool_region, pool_id) = pool(issuer);
    let metadata = fetch_oidc_metadata(issuer, ca_bundle)?;
    let authorize_endpoint = str_of(&metadata, "authorization_endpoint").unwrap_or_default();
    let token_endpoint = str_of(&metadata, "token_endpoint").unwrap_or_default();
    if authorize_endpoint.is_empty() || token_endpoint.is_empty() {
        return Err(Error::Jwt(format!(
            "Pool {pool_id} has no domain, so OIDC discovery advertises no sign-in endpoint. The registry owner must add a domain to the user pool (in the Amazon Cognito console: Branding, then Domain)."
        )));
    }
    let scopes = with_scopes(&["openid".to_string()], extra_scopes);
    let payload = run_browser_login(
        ctx,
        authorize_endpoint,
        token_endpoint,
        client_id,
        &scopes,
        ca_bundle,
        open_browser,
    )?;
    let token = str_of(&payload, "access_token")
        .unwrap_or_default()
        .to_string();
    save_token(
        endpoint,
        registry_id,
        &Saved {
            token: &token,
            expires_in: payload.get("expires_in").cloned(),
            refresh_token: str_of(&payload, "refresh_token").map(str::to_string),
            client_id: Some(client_id.to_string()),
            issuer: Some(issuer.to_string()),
            revocation_endpoint: revocation_endpoint(&metadata),
            pool_region: Some(pool_region),
            token_endpoint: None,
            scopes: granted_scopes(&payload, &scopes),
        },
    );
    Ok(token)
}

/// `CognitoProvider.refresh_endpoint(cached)`.
fn refresh_endpoint(cached: &Value) -> Option<String> {
    let issuer = str_of(cached, "issuer")?;
    let c = COGNITO_ISSUER.captures(issuer.trim_end_matches('/'))?;
    if Some(&c[1]) != str_of(cached, "poolRegion") {
        return None;
    }
    Some(url_origin(issuer))
}

/// `CognitoProvider.refresh(session, cached, ca_bundle)`: Cognito's
/// unauthenticated `GetTokensFromRefreshToken`, at the pool that issued
/// the token.
pub(crate) fn cognito_refresh(
    ctx: &Context,
    cached: &Value,
    ca_bundle: Option<&str>,
) -> std::result::Result<cache::Refreshed, RefreshError> {
    let Some(endpoint_url) = refresh_endpoint(cached) else {
        crate::debuglog::debug(
            "awscli.customizations.agentregistry.providers",
            "Cached Cognito sign-in has no usable issuer; not refreshing",
        );
        return Ok((None, None, None));
    };
    let region = str_of(cached, "poolRegion").unwrap_or_default().to_string();
    let missing = |k: &str| RefreshError::Other(crate::pyfmt::str_repr(k));
    let client_id = cached
        .get("clientId")
        .cloned()
        .ok_or_else(|| missing("clientId"))?;
    let refresh_token = cached
        .get("refreshToken")
        .cloned()
        .ok_or_else(|| missing("refreshToken"))?;
    let sub = cognito_context(ctx, &endpoint_url, &region, ca_bundle)
        .map_err(|e| RefreshError::Other(e.message))?;
    let client = crate::commands::services::exec::Client::with_config(
        &sub,
        "cognito-idp",
        crate::driver::ClientConfig::default(),
    )
    .map_err(|e| RefreshError::Other(e.message))?;
    let mut params = Map::new();
    params.insert("ClientId".to_string(), client_id);
    params.insert("RefreshToken".to_string(), refresh_token);
    let response = match client.call_raw("GetTokensFromRefreshToken", &Value::Map(params)) {
        Ok(crate::driver::CallResult::Ok(v)) => v,
        Ok(crate::driver::CallResult::ServiceError(info)) => {
            return Err(RefreshError::Client {
                code: info.code.clone(),
                message: info.exception_message(),
            });
        }
        Err(e) => return Err(RefreshError::Other(e.to_string())),
    };
    let result = match response.get("AuthenticationResult") {
        Some(r) if r.is_truthy() => r.clone(),
        _ => Value::map(),
    };
    let mut selected = Map::new();
    selected.insert(
        "access_token".to_string(),
        result.get("AccessToken").cloned().unwrap_or(Value::Null),
    );
    selected.insert(
        "id_token".to_string(),
        result.get("IdToken").cloned().unwrap_or(Value::Null),
    );
    let (token, _) =
        select_bearer_token(&Value::Map(selected)).map_err(|e| RefreshError::Other(e.message()))?;
    Ok((
        Some(token),
        result.get("ExpiresIn").cloned(),
        str_of(&result, "RefreshToken").map(str::to_string),
    ))
}

/// The context of the Cognito client: `region_name`, `endpoint_url`,
/// `signature_version=UNSIGNED`, and `verify=ca_bundle` (the option to
/// skip certificate verification does not apply).
fn cognito_context(
    ctx: &Context,
    endpoint_url: &str,
    region: &str,
    ca_bundle: Option<&str>,
) -> std::result::Result<Context, crate::error::CliError> {
    let mut globals = ctx.globals.clone();
    globals.endpoint_url = Some(endpoint_url.to_string());
    globals.region = Some(region.to_string());
    globals.no_sign_request = true;
    globals.no_verify_ssl = false;
    globals.ca_bundle = ca_bundle.map(str::to_string);
    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let session =
        crate::session::Session::with_env(&globals, env).map_err(|e| ctx.session_error(&e))?;
    Ok(Context {
        globals,
        session,
        runtime: crate::driver::LazyRuntime::default(),
        terminal: crate::driver::Terminal {
            stdout_is_tty: ctx.terminal.stdout_is_tty,
            width: ctx.terminal.width,
        },
    })
}
