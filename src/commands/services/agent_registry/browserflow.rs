//! `awscli/customizations/agentregistry/browserflow.py`: the OAuth
//! authorization-code flow with PKCE against the CLI's loopback listener
//! (`AuthCodeFetcher`) and sign-in prompt (`OpenBrowserHandler`,
//! `PrintOnlyHandler`).

use std::sync::LazyLock;
use std::time::Duration;

use base64::Engine as _;
use regex::Regex;
use sha2::{Digest, Sha256};

use super::constants::{
    CALLBACK_HOST, CALLBACK_PORT_ENV_VAR, DEFAULT_CALLBACK_PORT, HTTP_TIMEOUT, MAX_PRINTED_TEXT,
    MAX_RESPONSE_BYTES, TOKEN_ENV_VAR,
};
use super::urlreq::{self, Request, UrlError};
use super::utils::{
    json_loads, printable, printable_or_name, printable_url, read_limited, require_https_jwt,
    select_bearer_token, urlencode, write_status,
};
use super::{Error, Result};
use crate::commands::sso::flow::{AuthCodeFetcher, PendingHandler};
use crate::value::Value;

/// `_ERROR_BODY_BYTES`.
const ERROR_BODY_BYTES: usize = 4 * MAX_PRINTED_TEXT;

static PORT_DIGITS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A[0-9]{1,5}\z").expect("regex"));

/// `_OWNER_HINTS`.
fn owner_hint_text(error: &str) -> Option<&'static str> {
    Some(match error {
        "invalid_client" => {
            "The registry owner must configure the app client as a public client with no client secret."
        }
        "unauthorized_client" | "unsupported_response_type" => {
            "The app client does not allow the authorization-code grant; the registry owner must allow it."
        }
        "invalid_scope" => {
            "The app client does not allow the requested scopes ({scopes}); the registry owner must allow them."
        }
        _ => return None,
    })
}

/// `base64.urlsafe_b64encode(data).rstrip('=')`.
fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// `pkce_pair()`: `(verifier, challenge)`.
fn pkce_pair() -> (String, String) {
    let bytes: [u8; 32] = rand::random();
    let verifier = b64url(&bytes);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// `secrets.token_urlsafe(24)`.
fn token_urlsafe() -> String {
    let bytes: [u8; 24] = rand::random();
    b64url(&bytes)
}

/// `callback_port()`.
pub(crate) fn callback_port() -> Result<u16> {
    let Some(value) = std::env::var(CALLBACK_PORT_ENV_VAR)
        .ok()
        .filter(|v| !v.is_empty())
    else {
        return Ok(DEFAULT_CALLBACK_PORT);
    };
    let port: u32 = if PORT_DIGITS.is_match(&value) {
        value.parse().unwrap_or(0)
    } else {
        0
    };
    if !(1..=65535).contains(&port) {
        return Err(Error::Jwt(format!(
            "{CALLBACK_PORT_ENV_VAR} must be a port number from 1 to 65535, got {}.",
            crate::pyfmt::str_repr(&printable(&value))
        )));
    }
    Ok(port as u16)
}

/// `_listen(port)`.
fn listen(port: u16) -> Result<AuthCodeFetcher> {
    AuthCodeFetcher::bind(Some(port)).map_err(|e| {
        Error::Jwt(format!(
            "Could not listen on {CALLBACK_HOST}:{port} for the sign-in redirect ({e}). Free the port, or set {CALLBACK_PORT_ENV_VAR} to another port that is also registered as a callback URL with the identity provider."
        ))
    })
}

/// `_show_link(url, open_browser)`: on stderr.
fn show_link(ctx: &crate::driver::Context, url: &str, open_browser: bool) {
    let handler = if open_browser {
        PendingHandler::OpenBrowser
    } else {
        PendingHandler::PrintOnly
    };
    handler.call_to(ctx.session.config(), None, url, url, None, &|text| {
        crate::commands::services::basic::eprint(text)
    });
}

fn timeout_minutes(fetcher: &AuthCodeFetcher) -> u64 {
    fetcher.overall_timeout() / 60
}

/// `_timeout_message(fetcher, port, callback)`.
fn timeout_message(fetcher: &AuthCodeFetcher, port: u16, callback: &str) -> String {
    format!(
        "Timed out after {} minutes waiting for the sign-in redirect to {callback}. Either:\n  - The browser is not running on this machine. Over SSH, forward the port first (ssh -L {port}:{CALLBACK_HOST}:{port} <host>) and run the command again with --no-browser to open the printed link yourself, or export a token as {TOKEN_ENV_VAR}, which --authorizer JWT then sends as is.\n  - The sign-in page showed an error, for example a redirect mismatch or \"not allowed to access this app\". The registry owner must check the app client: the callback URL {callback} is registered, the authorization-code flow is allowed, and you are permitted to use it.",
        timeout_minutes(fetcher)
    )
}

/// `_owner_hint(error, scopes)`.
fn owner_hint(error: Option<&str>, scopes: &[String]) -> String {
    match error.and_then(owner_hint_text) {
        Some(hint) => {
            let listed = scopes.join(" ");
            let listed = if listed.is_empty() { "none" } else { &listed };
            format!(" {}", hint.replace("{scopes}", listed))
        }
        None => String::new(),
    }
}

/// `_token_error_hint(body, scopes)`.
fn token_error_hint(body: &str, scopes: &[String]) -> String {
    match crate::cli::pyjson::loads(body) {
        Ok(v @ Value::Map(_)) => match v.get("error") {
            Some(Value::Str(e)) => owner_hint(Some(e), scopes),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

const SIGN_IN_PARAMETERS: [&str; 7] = [
    "response_type",
    "client_id",
    "redirect_uri",
    "state",
    "code_challenge",
    "code_challenge_method",
    "scope",
];
const OVERRIDING_PARAMETERS: [&str; 3] = ["request", "request_uri", "response_mode"];

/// `urllib.parse.parse_qsl(query, keep_blank_values=True)`.
fn parse_qsl(query: &str) -> Vec<(String, String)> {
    let unquote = |s: &str| {
        percent_encoding::percent_decode_str(&s.replace('+', " "))
            .decode_utf8_lossy()
            .into_owned()
    };
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (unquote(k), unquote(v))
        })
        .collect()
}

/// `sign_in_link(authorize_endpoint, query)`.
pub(crate) fn sign_in_link(authorize_endpoint: &str, query: &[(&str, String)]) -> String {
    let parts = crate::session::url::urlsplit(authorize_endpoint);
    let kept: Vec<(String, String)> = parse_qsl(&parts.query)
        .into_iter()
        .filter(|(k, _)| {
            !SIGN_IN_PARAMETERS.contains(&k.as_str())
                && !OVERRIDING_PARAMETERS.contains(&k.as_str())
                && !query.iter().any(|(q, _)| q == k)
        })
        .collect();
    let mut pairs: Vec<(&str, &str)> = kept.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    pairs.extend(query.iter().map(|(k, v)| (*k, v.as_str())));
    crate::session::url::urlunsplit(&crate::session::url::SplitUrl {
        scheme: parts.scheme,
        netloc: parts.netloc,
        path: parts.path,
        query: urlencode(&pairs),
        fragment: String::new(),
    })
}

/// `_wait_for_code(fetcher, state, port, scopes)`.
fn wait_for_code(
    fetcher: &AuthCodeFetcher,
    state: &str,
    port: u16,
    scopes: &[String],
) -> Result<String> {
    let callback = fetcher.redirect_uri_with_port();
    let Some(result) = fetcher.wait(state) else {
        return Err(Error::Jwt(timeout_message(fetcher, port, &callback)));
    };
    if let Some((error, description)) = &result.error {
        let message = format!(
            "Sign-in failed: {} {}",
            printable(error),
            printable(description.as_deref().unwrap_or(""))
        );
        return Err(Error::Jwt(format!(
            "{}{}",
            super::utils::py_strip(&message),
            owner_hint(Some(error), scopes)
        )));
    }
    Ok(result.code.unwrap_or_default())
}

/// `run_browser_login(...)`: the token response, with `access_token` the
/// token the registry can validate and `token_source` the field it came
/// from.
pub(crate) fn run_browser_login(
    ctx: &crate::driver::Context,
    authorize_endpoint: &str,
    token_endpoint: &str,
    client_id: &str,
    scopes: &[String],
    ca_bundle: Option<&str>,
    open_browser: bool,
) -> Result<Value> {
    require_https_jwt(authorize_endpoint, "authorization endpoint")?;
    require_https_jwt(token_endpoint, "token endpoint")?;

    let (verifier, challenge) = pkce_pair();
    let state = token_urlsafe();
    let port = callback_port()?;
    let fetcher = listen(port)?;
    let callback = fetcher.redirect_uri_with_port();
    let mut query: Vec<(&str, String)> = vec![
        ("response_type", "code".to_string()),
        ("client_id", client_id.to_string()),
        ("redirect_uri", callback.clone()),
        ("state", state.clone()),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256".to_string()),
    ];
    if !scopes.is_empty() {
        query.push(("scope", scopes.join(" ")));
    }
    show_link(ctx, &sign_in_link(authorize_endpoint, &query), open_browser);
    write_status(&format!(
        "Waiting for sign-in to complete (up to {} minutes)...",
        timeout_minutes(&fetcher)
    ));
    let code = wait_for_code(&fetcher, &state, port, scopes)?;
    drop(fetcher);

    let form = urlencode(&[
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("code", &code),
        ("redirect_uri", &callback),
        ("code_verifier", &verifier),
    ]);
    let exchanged = urlreq::urlopen(
        &Request {
            url: token_endpoint,
            data: Some(form.as_bytes()),
            headers: &[("Content-Type", "application/x-www-form-urlencoded")],
        },
        Duration::from_secs(HTTP_TIMEOUT),
        "token endpoint",
        ca_bundle,
    )
    .and_then(|mut r| read_limited(&mut r, token_endpoint, MAX_RESPONSE_BYTES));
    let shown = printable_url(token_endpoint);
    let body = match exchanged {
        Ok(b) => b,
        Err(UrlError::Http(e)) => {
            let body = e.read_error_body(ERROR_BODY_BYTES);
            return Err(Error::Jwt(format!(
                "Code exchange failed at {shown} (HTTP {}): {}{}",
                e.code,
                printable(&body),
                token_error_hint(&body, scopes)
            )));
        }
        Err(UrlError::Jwt(m)) => return Err(Error::Jwt(m)),
        Err(e) => {
            return Err(Error::Jwt(format!(
                "Code exchange failed at {shown}: {}",
                printable_or_name(&e.message(), e.type_name())
            )));
        }
    };
    let mut payload = json_loads(&body).map_err(|e| {
        Error::Jwt(format!(
            "Code exchange failed at {shown}: {}",
            printable_or_name(&e, "JSONDecodeError")
        ))
    })?;
    let (token, source) = select_bearer_token(&payload)?;
    if let Some(m) = payload.as_map_mut() {
        m.insert("access_token".to_string(), Value::str(token));
        m.insert("token_source".to_string(), Value::str(source));
    }
    write_status("Signed in.\n");
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_merges_the_endpoint_query() {
        let link = sign_in_link(
            "https://idp.example/authorize?tenant=a+b&state=evil&response_mode=form_post#frag",
            &[
                ("response_type", "code".to_string()),
                ("scope", "openid profile".to_string()),
            ],
        );
        assert_eq!(
            link,
            "https://idp.example/authorize?tenant=a+b&response_type=code&scope=openid+profile"
        );
    }
}
