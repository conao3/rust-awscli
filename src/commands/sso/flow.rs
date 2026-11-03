//! The SSO login flows (`awscli/customizations/sso/utils.py`
//! `do_sso_login`, `botocore/utils.py` `SSOTokenFetcher` (device
//! authorization) and `SSOTokenFetcherAuth` (authorization code with
//! PKCE)), the loopback callback server (`AuthCodeFetcher`), the browser
//! handlers, and start URL resolution (`resolve.py`).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use base64::Engine;
use rand::Rng;
use sha2::{Digest, Sha256};

use crate::commands::configure::out;
use crate::config::Config;
use crate::session::cache::{self, JsonFileCache};
use crate::session::client::{ApiClient, ApiError, percent_encode_sequence};
use crate::session::error::{ErrorKind, SessionError};
use crate::session::pydatetime::{self, PyDateTime};
use crate::session::sso_token::{registration_cache_key, token_cache_key};
use crate::value::{Map, Value};

const INDEX_HTML: &[u8] = include_bytes!("index.html");

fn general(msg: impl Into<String>) -> SessionError {
    SessionError::new(ErrorKind::General, msg)
}

fn configuration(msg: impl Into<String>) -> SessionError {
    SessionError::new(ErrorKind::Configuration, msg)
}

/// What to do when the user must act in the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingHandler {
    /// `OpenBrowserHandler(open_browser=open_browser_with_original_ld_path)`.
    OpenBrowser,
    /// `PrintOnlyHandler`.
    PrintOnly,
}

impl PendingHandler {
    /// `__call__(userCode, verificationUri, verificationUriComplete,
    /// cross_device_flag)`.
    pub(crate) fn call(
        self,
        config: &Config,
        user_code: Option<&str>,
        uri: &str,
        uri_complete: &str,
        cross_device_flag: Option<&str>,
    ) {
        match self {
            PendingHandler::PrintOnly => {
                out(&format!(
                    "Browser will not be automatically opened.\nPlease visit the following URL:\n\n{uri}\n"
                ));
                if let Some(code) = user_code.filter(|c| !c.is_empty()) {
                    out(&format!(
                        "\nThen enter the code:\n\n{code}\n\nAlternatively, you may visit the following URL which will autofill the code upon loading:\n{uri_complete}\n"
                    ));
                }
            }
            PendingHandler::OpenBrowser => {
                let has_code = user_code.is_some_and(|c| !c.is_empty());
                if has_code {
                    out(&format!(
                        "Attempting to open your default browser.\nIf the browser does not open or you wish to use a different device to authorize this request, open the following URL:\n\n{uri}\n"
                    ));
                    out(&format!(
                        "\nThen enter the code:\n\n{}\n",
                        user_code.unwrap_or_default()
                    ));
                } else {
                    out(&format!(
                        "Attempting to open your default browser. If the browser does not open, open the following URL.\nIf you are unable to open the URL on this device, run this command again with the '{}' option.\n\n{uri}\n\n",
                        cross_device_flag.unwrap_or("None")
                    ));
                }
                open_browser(config, uri_complete);
            }
        }
    }
}

/// `webbrowser.open_new_tab(url)` with `original_ld_library_path`: the
/// `BROWSER` entries, then the graphical openers when a display is set,
/// then the console browsers, until one succeeds.
pub(crate) fn open_browser(config: &Config, url: &str) {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(b) = config.env("BROWSER") {
        candidates.extend(b.split(':').filter(|s| !s.is_empty()).map(str::to_string));
    }
    if config.env("DISPLAY").is_some() || config.env("WAYLAND_DISPLAY").is_some() {
        for c in [
            "xdg-open",
            "gio",
            "x-www-browser",
            "firefox",
            "chromium",
            "google-chrome",
        ] {
            candidates.push(c.to_string());
        }
    }
    for c in ["www-browser", "links", "elinks", "lynx", "w3m"] {
        candidates.push(c.to_string());
    }
    for candidate in candidates {
        let Some(mut parts) = crate::config::ini::shlex_split(&candidate) else {
            continue;
        };
        if parts.is_empty() {
            continue;
        }
        let background = candidate.trim_end().ends_with('&');
        if background {
            parts.retain(|p| p != "&");
        }
        let mut replaced = false;
        for p in parts.iter_mut() {
            if p.contains("%s") {
                *p = p.replace("%s", url);
                replaced = true;
            }
        }
        if !replaced {
            parts.push(url.to_string());
        }
        let program = if candidate == "gio" {
            parts.insert(1, "open".to_string());
            parts[0].clone()
        } else {
            parts[0].clone()
        };
        let mut cmd = std::process::Command::new(&program);
        cmd.args(&parts[1..]);
        crate::session::credentials::apply_original_ld_library_path(config, &mut cmd);
        let Ok(mut child) = cmd.spawn() else {
            continue;
        };
        if background {
            return;
        }
        if child.wait().is_ok_and(|s| s.success()) {
            return;
        }
    }
}

/// `AuthCodeFetcher`: the loopback HTTP server for the OAuth callback.
pub(crate) struct AuthCodeFetcher {
    listener: TcpListener,
    port: u16,
}

impl AuthCodeFetcher {
    pub(crate) fn new(redirect_port: Option<u16>) -> Result<AuthCodeFetcher, SessionError> {
        let listener = TcpListener::bind(("127.0.0.1", redirect_port.unwrap_or(0))).map_err(|e| {
            general(format!(
                "Unable to initialize the OAuth 2.0 authorization callback handler: {} \n You may use --use-device-code to fall back to the device code flow which does not require the callback handler.",
                os_error_text(&e)
            ))
        })?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or_default();
        Ok(AuthCodeFetcher { listener, port })
    }

    pub(crate) fn redirect_uri_without_port(&self) -> String {
        "http://127.0.0.1/oauth/callback".to_string()
    }

    pub(crate) fn redirect_uri_with_port(&self) -> String {
        format!("http://127.0.0.1:{}/oauth/callback", self.port)
    }

    /// `get_auth_code_and_state`: serve requests until one carries the
    /// code and state (or an error), for at most ten minutes.
    pub(crate) fn get_auth_code_and_state(
        &self,
    ) -> Result<(Option<String>, Option<String>), SessionError> {
        let start = Instant::now();
        let _ = self.listener.set_nonblocking(true);
        while start.elapsed() < Duration::from_secs(600) {
            let stream = match self.listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(_) => continue,
            };
            if let Some(result) = handle_callback(stream) {
                return Ok(result);
            }
        }
        Err(general(
            "The pending authorization to retrieve an SSO token has expired. The login flow to retrieve an SSO token must be restarted.",
        ))
    }
}

fn os_error_text(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => format!(
            "[Errno {code}] {}",
            std::io::Error::from_raw_os_error(code)
                .to_string()
                .split(" (os error")
                .next()
                .unwrap_or_default()
        ),
        None => e.to_string(),
    }
}

/// `OAuthCallbackHandler.do_GET`.
fn handle_callback(mut stream: TcpStream) -> Option<(Option<String>, Option<String>)> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && !buf.windows(2).any(|w| w == b"\n\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if buf.len() > 65536 {
            break;
        }
    }
    let request = String::from_utf8_lossy(&buf);
    let line = request.lines().next().unwrap_or_default();
    let mut words = line.split_whitespace();
    let method = words.next().unwrap_or_default();
    let path = words.next().unwrap_or_default();
    if method != "GET" {
        let _ = stream.write_all(b"HTTP/1.0 501 Unsupported method\r\n\r\n");
        return None;
    }
    let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT");
    let mut response = format!(
        "HTTP/1.0 200 OK\r\nServer: AWS CLI/{}\r\nDate: {date}\r\n\r\n",
        crate::model::REFERENCE_VERSION.trim()
    )
    .into_bytes();
    response.extend_from_slice(INDEX_HTML);
    let _ = stream.write_all(&response);
    let _ = stream.flush();
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or_default();
    let query = query.split('#').next().unwrap_or_default();
    let params = parse_qs(query);
    if params.iter().any(|(k, _)| k == "error") {
        return Some((None, None));
    }
    let code = params
        .iter()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.clone());
    let state = params
        .iter()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.clone());
    match (code, state) {
        (Some(c), Some(s)) => Some((Some(c), Some(s))),
        _ => None,
    }
}

/// `parse_qs` (first value of each key; blank values dropped).
pub(crate) fn parse_qs(query: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if v.is_empty() {
            continue;
        }
        let decode = |s: &str| {
            percent_encoding::percent_decode_str(&s.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned()
        };
        let k = decode(k);
        if !out.iter().any(|(x, _)| *x == k) {
            out.push((k, decode(v)));
        }
    }
    out
}

/// The arguments of `do_sso_login`.
pub(crate) struct SsoLogin {
    pub sso_region: String,
    pub start_url: String,
    pub resolved_start_url: Option<String>,
    pub session_name: Option<String>,
    pub registration_scopes: Option<Vec<String>>,
    pub force_refresh: bool,
    pub use_device_code: bool,
    pub redirect_port: Option<u16>,
    pub handler: PendingHandler,
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

/// `BaseSSOTokenFetcher._is_expired`.
fn is_expired(v: &Value) -> bool {
    str_of(v, "expiresAt")
        .and_then(PyDateTime::parse)
        .is_none_or(|e| e.seconds_remaining() < 15.0 * 60.0)
}

fn is_auth_code_registration(v: &Value) -> bool {
    v.get("grantTypes")
        .and_then(Value::as_list)
        .is_some_and(|l| l.iter().any(|g| g.as_str() == Some("authorization_code")))
}

/// `PKCE` code verifier and challenge.
pub(crate) fn pkce() -> (String, String) {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~";
    let mut rng = rand::rng();
    let verifier: String = (0..64)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect();
    let challenge =
        base64::engine::general_purpose::URL_SAFE.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn client_name(session_name: Option<&str>) -> String {
    match session_name {
        Some(s) => format!("botocore-client-{s}"),
        None => format!("botocore-client-{}", chrono::Utc::now().timestamp()),
    }
}

fn api_err(e: ApiError) -> SessionError {
    e.into_session_error()
}

/// `do_sso_login`: the cached or new SSO token.
pub(crate) async fn do_sso_login(config: &Config, args: &SsoLogin) -> Result<Value, SessionError> {
    let token_cache = cache::sso_cache(config);
    let auth_fetcher = if args.session_name.is_some() && !args.use_device_code {
        Some(AuthCodeFetcher::new(args.redirect_port)?)
    } else {
        None
    };
    let client = ApiClient::new(
        config,
        "sso-oidc",
        Some(args.sso_region.clone()),
        None,
        None,
    )?;
    let key = token_cache_key(&args.start_url, args.session_name.as_deref());
    if !args.force_refresh
        && let Some(token) = token_cache.get(&key)
        && !is_expired(&token)
    {
        return Ok(token);
    }
    let token = match &auth_fetcher {
        Some(fetcher) => auth_code_token(config, &client, &token_cache, args, fetcher).await?,
        None => device_code_token(config, &client, &token_cache, args).await?,
    };
    let _ = token_cache.set(&key, &token);
    Ok(token)
}

fn token_value(args: &SsoLogin, response: &Value, registration: &Value) -> Value {
    let mut token = Map::new();
    token.insert("startUrl".into(), Value::str(&args.start_url));
    token.insert("region".into(), Value::str(&args.sso_region));
    token.insert(
        "accessToken".into(),
        response.get("accessToken").cloned().unwrap_or(Value::Null),
    );
    let expires_in = response
        .get("expiresIn")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    token.insert(
        "expiresAt".into(),
        Value::str(pydatetime::utc_after(expires_in)),
    );
    for (k, src) in [
        ("clientId", "clientId"),
        ("clientSecret", "clientSecret"),
        ("registrationExpiresAt", "expiresAt"),
    ] {
        token.insert(
            k.into(),
            registration.get(src).cloned().unwrap_or(Value::Null),
        );
    }
    if let Some(r) = response.get("refreshToken") {
        token.insert("refreshToken".into(), r.clone());
    }
    Value::Map(token)
}

fn scopes_value(scopes: &[String]) -> Value {
    Value::List(scopes.iter().map(Value::str).collect())
}

/// `SSOTokenFetcher._poll_for_token`.
async fn device_code_token(
    config: &Config,
    client: &ApiClient<'_>,
    cache: &JsonFileCache,
    args: &SsoLogin,
) -> Result<Value, SessionError> {
    let scopes = args.registration_scopes.as_deref();
    let reg_key = registration_cache_key(
        &args.start_url,
        &args.sso_region,
        scopes,
        args.session_name.as_deref(),
    );
    let registration = match cache.get(&reg_key) {
        Some(r) if !is_expired(&r) && !is_auth_code_registration(&r) => r,
        _ => {
            let mut params = Map::new();
            params.insert(
                "clientName".into(),
                Value::str(client_name(args.session_name.as_deref())),
            );
            params.insert("clientType".into(), Value::str("public"));
            if let Some(s) = scopes.filter(|s| !s.is_empty()) {
                params.insert("scopes".into(), scopes_value(s));
            }
            let response = client
                .call("RegisterClient", &Value::Map(params), None)
                .await
                .map_err(api_err)?;
            let mut reg = Map::new();
            reg.insert(
                "clientId".into(),
                response.get("clientId").cloned().unwrap_or(Value::Null),
            );
            reg.insert(
                "clientSecret".into(),
                response.get("clientSecret").cloned().unwrap_or(Value::Null),
            );
            let expires = response
                .get("clientSecretExpiresAt")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            reg.insert(
                "expiresAt".into(),
                Value::str(pydatetime::utc_from_timestamp(expires as f64)),
            );
            if let Some(s) = scopes.filter(|s| !s.is_empty()) {
                reg.insert("scopes".into(), scopes_value(s));
            }
            let reg = Value::Map(reg);
            let _ = cache.set(&reg_key, &reg);
            reg
        }
    };
    let mut params = Map::new();
    params.insert(
        "clientId".into(),
        registration.get("clientId").cloned().unwrap_or(Value::Null),
    );
    params.insert(
        "clientSecret".into(),
        registration
            .get("clientSecret")
            .cloned()
            .unwrap_or(Value::Null),
    );
    params.insert(
        "startUrl".into(),
        Value::str(
            args.resolved_start_url
                .as_deref()
                .unwrap_or(&args.start_url),
        ),
    );
    let auth = client
        .call("StartDeviceAuthorization", &Value::Map(params), None)
        .await
        .map_err(api_err)?;
    let mut interval = auth.get("interval").and_then(Value::as_i64).unwrap_or(5);
    let mut token_params = Map::new();
    token_params.insert(
        "grantType".into(),
        Value::str("urn:ietf:params:oauth:grant-type:device_code"),
    );
    token_params.insert(
        "clientId".into(),
        registration.get("clientId").cloned().unwrap_or(Value::Null),
    );
    token_params.insert(
        "clientSecret".into(),
        registration
            .get("clientSecret")
            .cloned()
            .unwrap_or(Value::Null),
    );
    token_params.insert(
        "deviceCode".into(),
        auth.get("deviceCode").cloned().unwrap_or(Value::Null),
    );
    let token_params = Value::Map(token_params);
    let mut first = true;
    loop {
        match client.call("CreateToken", &token_params, None).await {
            Ok(response) => return Ok(token_value(args, &response, &registration)),
            Err(ApiError::Service(f)) if f.code == "SlowDownException" => interval += 5,
            Err(ApiError::Service(f)) if f.code == "AuthorizationPendingException" => {}
            Err(ApiError::Service(f)) if f.code == "ExpiredTokenException" => {
                return Err(general(
                    "The pending authorization to retrieve an SSO token has expired. The login flow to retrieve an SSO token must be restarted.",
                ));
            }
            Err(e) => return Err(api_err(e)),
        }
        if first {
            first = false;
            args.handler.call(
                config,
                str_of(&auth, "userCode"),
                str_of(&auth, "verificationUri").unwrap_or_default(),
                str_of(&auth, "verificationUriComplete").unwrap_or_default(),
                None,
            );
            continue;
        }
        tokio::time::sleep(Duration::from_secs(interval.max(0) as u64)).await;
    }
}

/// `SSOTokenFetcherAuth._get_new_token`.
async fn auth_code_token(
    config: &Config,
    client: &ApiClient<'_>,
    cache: &JsonFileCache,
    args: &SsoLogin,
    fetcher: &AuthCodeFetcher,
) -> Result<Value, SessionError> {
    let (verifier, challenge) = pkce();
    let scopes = args.registration_scopes.as_deref();
    let reg_key = registration_cache_key(
        &args.start_url,
        &args.sso_region,
        scopes,
        args.session_name.as_deref(),
    );
    let registration = match cache.get(&reg_key) {
        Some(r) if !is_expired(&r) && is_auth_code_registration(&r) => r,
        _ => {
            let default_scopes = vec!["sso:account:access".to_string()];
            let used_scopes = scopes.filter(|s| !s.is_empty()).unwrap_or(&default_scopes);
            let grant_types = Value::List(vec![
                Value::str("authorization_code"),
                Value::str("refresh_token"),
            ]);
            let mut params = Map::new();
            params.insert(
                "clientName".into(),
                Value::str(client_name(args.session_name.as_deref())),
            );
            params.insert("clientType".into(), Value::str("public"));
            params.insert("grantTypes".into(), grant_types.clone());
            params.insert(
                "redirectUris".into(),
                Value::List(vec![Value::str(fetcher.redirect_uri_without_port())]),
            );
            params.insert(
                "issuerUrl".into(),
                Value::str(
                    args.resolved_start_url
                        .as_deref()
                        .unwrap_or(&args.start_url),
                ),
            );
            params.insert("scopes".into(), scopes_value(used_scopes));
            let response = client
                .call("RegisterClient", &Value::Map(params), None)
                .await
                .map_err(api_err)?;
            let expires = response
                .get("clientSecretExpiresAt")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let mut reg = Map::new();
            reg.insert(
                "clientId".into(),
                response.get("clientId").cloned().unwrap_or(Value::Null),
            );
            reg.insert(
                "clientSecret".into(),
                response.get("clientSecret").cloned().unwrap_or(Value::Null),
            );
            reg.insert(
                "expiresAt".into(),
                Value::str(pydatetime::utc_from_timestamp(expires as f64)),
            );
            reg.insert("scopes".into(), scopes_value(used_scopes));
            reg.insert("grantTypes".into(), grant_types);
            let reg = Value::Map(reg);
            let _ = cache.set(&reg_key, &reg);
            reg
        }
    };
    let state = uuid::Uuid::new_v4().to_string();
    let mut probe = Map::new();
    probe.insert("clientName".into(), Value::str("temp"));
    probe.insert("clientType".into(), Value::str("public"));
    let probe_url = client
        .request_url("RegisterClient", &Value::Map(probe))
        .map_err(api_err)?;
    let base = base_uri(&probe_url);
    let scope_text = match scopes.filter(|s| !s.is_empty()) {
        Some(s) => s.join(" "),
        None => "sso:account:access".to_string(),
    };
    let query = percent_encode_sequence(&[
        ("response_type", "code".to_string()),
        (
            "client_id",
            str_of(&registration, "clientId")
                .unwrap_or_default()
                .to_string(),
        ),
        ("redirect_uri", fetcher.redirect_uri_with_port()),
        ("state", state.clone()),
        ("code_challenge_method", "S256".to_string()),
        ("scopes", scope_text),
    ]);
    let uri = format!(
        "{base}/authorize?{query}&code_challenge={}",
        &challenge[..challenge.len() - 1]
    );
    args.handler
        .call(config, None, &uri, &uri, Some("--use-device-code"));
    let (code, got_state) = fetcher.get_auth_code_and_state()?;
    let Some(code) = code else {
        return Err(general(
            "Error loading authorization code: Failed to retrieve an authorization code.",
        ));
    };
    if got_state.as_deref() != Some(state.as_str()) {
        return Err(general(
            "Error loading authorization code: State parameter does not match expected value.",
        ));
    }
    let mut params = Map::new();
    params.insert("grantType".into(), Value::str("authorization_code"));
    params.insert(
        "clientId".into(),
        registration.get("clientId").cloned().unwrap_or(Value::Null),
    );
    params.insert(
        "clientSecret".into(),
        registration
            .get("clientSecret")
            .cloned()
            .unwrap_or(Value::Null),
    );
    params.insert(
        "redirectUri".into(),
        Value::str(fetcher.redirect_uri_with_port()),
    );
    params.insert("codeVerifier".into(), Value::str(verifier));
    params.insert("code".into(), Value::str(code));
    match client.call("CreateToken", &Value::Map(params), None).await {
        Ok(response) => Ok(token_value(args, &response, &registration)),
        Err(ApiError::Service(f)) if f.code == "ExpiredTokenException" => Err(general(
            "The pending authorization to retrieve an SSO token has expired. The login flow to retrieve an SSO token must be restarted.",
        )),
        Err(e) => Err(api_err(e)),
    }
}

/// `f'{parsed.scheme}://{parsed.netloc}'` of a URL.
pub(crate) fn base_uri(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or_default();
    format!("{scheme}://{netloc}")
}

const AWS_OWNED_SUFFIXES: &[&str] = &[
    ".app.aws",
    ".portal.amazonaws.com",
    ".portal.amazonaws.com.cn",
    ".app.amazonwebservices.com.cn",
    ".portal.amazonaws.eu",
    ".api.amazonwebservices.eu",
    ".awsapps.com",
    ".awsapps.cn",
];

const AWS_OWNED_EXACT: &[&str] = &[
    "identitycenter.amazonaws.com",
    "identitycenter.amazonaws.com.cn",
    "identitycenter.amazonaws.eu",
    "identitycenter.us-gov.amazonaws.com",
];

/// `is_aws_owned_domain`.
pub(crate) fn is_aws_owned_domain(hostname: &str) -> bool {
    let h = hostname.to_lowercase();
    let h = h.trim_end_matches('.');
    if AWS_OWNED_EXACT.contains(&h) {
        return true;
    }
    AWS_OWNED_SUFFIXES
        .iter()
        .any(|s| h == s.trim_start_matches('.') || h.ends_with(s))
}

/// `urlparse(url).hostname` and scheme.
pub(crate) fn url_host(url: &str) -> (String, Option<String>) {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s.to_lowercase(), r),
        None => (String::new(), ""),
    };
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let hostport = netloc.rsplit('@').next().unwrap_or_default();
    let host = if let Some(stripped) = hostport.strip_prefix('[') {
        stripped.split(']').next().unwrap_or_default().to_string()
    } else {
        hostport.split(':').next().unwrap_or_default().to_string()
    };
    let host = host.to_lowercase();
    (scheme, (!host.is_empty()).then_some(host))
}

fn extract_region_from_hostname(hostname: &str) -> Option<String> {
    let h = hostname.to_lowercase();
    let h = h.trim_end_matches('.');
    let labels: Vec<&str> = h.split('.').collect();
    let region_ok = |r: &str| {
        !r.is_empty()
            && r.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    };
    let patterns: [(&[&str], usize); 6] = [
        (&["portal", "*", "app", "aws"], 2),
        (&["*", "portal", "amazonaws", "com"], 1),
        (&["*", "portal", "amazonaws", "com", "cn"], 1),
        (&["portal", "*", "app", "amazonwebservices", "com", "cn"], 2),
        (&["*", "portal", "amazonaws", "eu"], 1),
        (&["portal", "*", "api", "amazonwebservices", "eu"], 2),
    ];
    for (suffix, region_at) in patterns {
        if labels.len() != suffix.len() + 1 {
            continue;
        }
        let tail = &labels[1..];
        if tail
            .iter()
            .zip(suffix.iter())
            .all(|(l, p)| *p == "*" || l == p)
        {
            let region = labels[region_at];
            if region_ok(region) {
                return Some(region.to_string());
            }
        }
    }
    None
}

/// `resolve_start_url`: (resolved URL, region).
pub(crate) async fn resolve_start_url(
    config: &Config,
    start_url: &str,
    configured_region: Option<&str>,
) -> Result<(String, String), SessionError> {
    let (scheme, host) = url_host(start_url);
    if scheme != "https" {
        return Err(configuration(
            "The sso_start_url must use the https scheme.",
        ));
    }
    let Some(host) = host else {
        return Err(configuration(format!(
            "Invalid sso_start_url: '{start_url}'"
        )));
    };
    if is_aws_owned_domain(&host) {
        let Some(region) = configured_region.filter(|r| !r.is_empty()) else {
            return Err(configuration(
                "Missing required configuration: sso_region. Please run 'aws configure sso' to set it.",
            ));
        };
        return Ok((start_url.to_string(), region.to_string()));
    }
    let resolved = normalize_url(&follow_redirect(config, start_url).await?);
    let (resolved_scheme, resolved_host) = url_host(&resolved);
    if !resolved_host.as_deref().is_some_and(is_aws_owned_domain) {
        return Err(configuration(format!(
            "Could not resolve start URL '{start_url}' to an AWS-owned endpoint. Final URL: '{resolved}'"
        )));
    }
    if resolved_scheme != "https" {
        return Err(configuration(format!(
            "Resolved URL must use https. Got: '{resolved}'"
        )));
    }
    let region = match resolved_host
        .as_deref()
        .and_then(extract_region_from_hostname)
    {
        Some(r) => r,
        None => match configured_region.filter(|r| !r.is_empty()) {
            Some(r) => r.to_string(),
            None => {
                return Err(configuration(format!(
                    "Cannot determine region from start URL '{start_url}'. Please provide sso_region in your configuration."
                )));
            }
        },
    };
    Ok((resolved, region))
}

fn normalize_url(url: &str) -> String {
    let (scheme, host) = url_host(url);
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or_default();
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let port = netloc
        .rsplit('@')
        .next()
        .and_then(|h| h.rsplit_once(':'))
        .and_then(|(_, p)| p.parse::<u16>().ok());
    let default_port = match scheme.as_str() {
        "https" => Some(443),
        "http" => Some(80),
        _ => None,
    };
    let mut netloc = host.unwrap_or_default();
    if let Some(p) = port
        && Some(p) != default_port
    {
        netloc = format!("{netloc}:{p}");
    }
    let path = rest[netloc_len(rest)..]
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    format!("{scheme}://{netloc}{path}")
}

fn netloc_len(rest: &str) -> usize {
    rest.find(['/', '?', '#']).unwrap_or(rest.len())
}

/// `_follow_redirect` (at most one redirect).
async fn follow_redirect(config: &Config, url: &str) -> Result<String, SessionError> {
    let client = crate::session::http::HttpClient::new(crate::session::http::HttpConfig {
        verify: crate::session::http::Verify::Default,
        connect_timeout: Some(Duration::from_secs(10)),
        read_timeout: Some(Duration::from_secs(10)),
        env: config.env_map().clone(),
        proxy_use_forwarding_for_https: false,
    })?;
    let mut response = client
        .send("HEAD", url, &[], &[])
        .await
        .map_err(|e| configuration(format!("Failed to resolve start URL: {}", e.message)))?;
    if matches!(response.status, 405 | 501) {
        response = client
            .send("GET", url, &[], &[])
            .await
            .map_err(|e| configuration(format!("Failed to resolve start URL: {}", e.message)))?;
    }
    match response.status {
        301 | 302 | 303 | 307 | 308 => {
            let Some(location) = response.header("Location") else {
                return Err(configuration("Redirect response missing Location header."));
            };
            Ok(urljoin(url, location))
        }
        s if s >= 400 => Err(configuration(format!(
            "Failed to resolve start URL: HTTP {s}"
        ))),
        _ => Ok(url.to_string()),
    }
}

fn urljoin(base: &str, location: &str) -> String {
    if location.contains("://") {
        return location.to_string();
    }
    let base_root = base_uri(base);
    if location.starts_with('/') {
        format!("{base_root}{location}")
    } else {
        let dir = base.rsplit_once('/').map(|(d, _)| d).unwrap_or(base);
        format!("{dir}/{location}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_and_regions() {
        assert!(is_aws_owned_domain("d-123.awsapps.com"));
        assert!(!is_aws_owned_domain("example.com"));
        assert_eq!(
            extract_region_from_hostname("ssoins-1.portal.us-west-2.app.aws").as_deref(),
            Some("us-west-2")
        );
        assert_eq!(
            extract_region_from_hostname("x.eu-west-1.portal.amazonaws.com").as_deref(),
            Some("eu-west-1")
        );
        assert_eq!(
            normalize_url("https://x.app.aws:443/start/"),
            "https://x.app.aws/start"
        );
    }
}
