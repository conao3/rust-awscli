//! `awscli/customizations/agentregistry/utils.py`: discovery, prompting,
//! URL and token-shape helpers, and the sign-out hint.

use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;

use regex::Regex;

use super::constants::{
    HTTP_TIMEOUT, MAX_PRINTED_TEXT, MAX_RESPONSE_BYTES, OIDC_METADATA_PATH, PRM_PATH,
    TOKEN_ENV_VAR, WANTED_SCOPES,
};
use super::urlreq::{self, Request, Response, UrlError};
use super::{Error, Result};
use crate::endpoint::pyurl::urlsplit_parts;
use crate::value::Value;

/// `prompt(message)`: `input(message).strip()`.
pub(crate) fn prompt(message: &str) -> Result<String> {
    match crate::session::prompt::input(message) {
        Ok(answer) => Ok(answer.trim().to_string()),
        Err(e) => Err(Error::Other(e.message().to_string())),
    }
}

/// `stdin_is_terminal()`.
pub(crate) fn stdin_is_terminal() -> bool {
    use std::io::IsTerminal as _;
    std::io::stdin().is_terminal()
}

/// `write_status(message)`: one line on stderr.
pub(crate) fn write_status(message: &str) {
    crate::commands::services::basic::eprint(&format!("{message}\n"));
}

/// `_CONTROL_CHARACTERS`.
static CONTROL_CHARACTERS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        "[\\x00-\\x1f\\x7f-\\u{9f}\\u{200e}\\u{200f}\\u{202a}-\\u{202e}\\u{2066}-\\u{2069}]+",
    )
    .expect("regex")
});

/// `str.strip()`.
pub(crate) fn py_strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}

/// `printable(text, limit)`: control characters replaced with a space,
/// stripped, and cut off after `limit` characters.
pub(crate) fn printable_limit(text: &str, limit: usize) -> String {
    let replaced = CONTROL_CHARACTERS.replace_all(text, " ");
    let text = py_strip(&replaced);
    if text.chars().count() > limit {
        let cut: String = text.chars().take(limit).collect();
        format!("{cut}...")
    } else {
        text.to_string()
    }
}

/// `printable(text)` with `MAX_PRINTED_TEXT`.
pub(crate) fn printable(text: &str) -> String {
    printable_limit(text, MAX_PRINTED_TEXT)
}

/// `url_origin(url)`: `scheme://host[:port]`.
pub(crate) fn url_origin(url: &str) -> String {
    let Ok(parts) = urlsplit_parts(url) else {
        return String::new();
    };
    let mut host = parts.hostname().unwrap_or_default();
    if let Ok(Some(port)) = parts.port()
        && port != 0
    {
        host = format!("{host}:{port}");
    }
    if parts.scheme.is_empty() {
        host
    } else {
        format!("{}://{host}", parts.scheme)
    }
}

/// `comparable_endpoint(url)`: scheme, host (with a port other than the
/// scheme's default), and the path without trailing slashes.
pub(crate) fn comparable_endpoint(url: &str) -> (String, String, String) {
    let Ok(parts) = urlsplit_parts(url) else {
        return (String::new(), String::new(), String::new());
    };
    let scheme = parts.scheme.to_lowercase();
    let port = parts.port().ok().flatten();
    let mut host = parts.hostname().unwrap_or_default();
    let default = match scheme.as_str() {
        "https" => Some(443),
        "http" => Some(80),
        _ => None,
    };
    if let Some(port) = port
        && Some(port) != default
    {
        host = format!("{host}:{port}");
    }
    (scheme, host, parts.path.trim_end_matches('/').to_string())
}

/// `printable_url(url)`: only the origin of a URL with a query.
pub(crate) fn printable_url(url: &str) -> String {
    match urlsplit_parts(url) {
        Ok(p) if !p.query.is_empty() => url_origin(url),
        _ => url.to_string(),
    }
}

/// `require_https(url, purpose)`: `Err` is the refusal.
pub(crate) fn require_https(url: &str, purpose: &str) -> std::result::Result<String, String> {
    let ok = match urlsplit_parts(url) {
        Ok(p) => {
            p.scheme == "https"
                && p.hostname().is_some_and(|h| !h.is_empty())
                && !CONTROL_CHARACTERS.is_match(url)
        }
        Err(_) => false,
    };
    if ok {
        Ok(url.to_string())
    } else {
        Err(format!(
            "Refusing to use {purpose} {}: only https URLs are allowed.",
            crate::pyfmt::str_repr(&printable_url(url))
        ))
    }
}

/// [`require_https`] as an [`Error`].
pub(crate) fn require_https_jwt(url: &str, purpose: &str) -> Result<String> {
    require_https(url, purpose).map_err(Error::Jwt)
}

/// `read_limited(response, url, limit)`.
pub(crate) fn read_limited(
    response: &mut Response,
    url: &str,
    limit: usize,
) -> std::result::Result<Vec<u8>, UrlError> {
    let body = response.read(limit + 1)?;
    if body.len() > limit {
        return Err(UrlError::Jwt(format!(
            "The response from {} is too large: more than {limit} bytes, so it was not read.",
            printable_url(url)
        )));
    }
    Ok(body)
}

/// `json.loads(body.decode('utf-8'))`: `Err` is the `ValueError`.
pub(crate) fn json_loads(body: &[u8]) -> std::result::Result<Value, String> {
    let text = decode_utf8(body)?;
    crate::cli::pyjson::loads(&text)
}

/// `bytes.decode('utf-8')`: `Err` is `str(UnicodeDecodeError)`.
pub(crate) fn decode_utf8(body: &[u8]) -> std::result::Result<String, String> {
    match std::str::from_utf8(body) {
        Ok(s) => Ok(s.to_string()),
        Err(e) => {
            let pos = e.valid_up_to();
            let byte = body[pos];
            let reason = match e.error_len() {
                None => "unexpected end of data",
                Some(_) if (0x80..0xc2).contains(&byte) || byte >= 0xf5 => "invalid start byte",
                Some(_) => "invalid continuation byte",
            };
            match e.error_len() {
                None if body.len() - pos > 1 => Err(format!(
                    "'utf-8' codec can't decode bytes in position {pos}-{}: {reason}",
                    body.len() - 1
                )),
                _ => Err(format!(
                    "'utf-8' codec can't decode byte 0x{byte:02x} in position {pos}: {reason}"
                )),
            }
        }
    }
}

/// `oauth_error_code(body)`.
pub(crate) fn oauth_error_code(body: &str) -> Option<String> {
    let document = crate::cli::pyjson::loads(body).ok()?;
    match document.get("error") {
        Some(Value::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

/// What `http_get_json` and `http_post_form` raise: the `urllib`
/// exceptions, or the `ValueError` of the JSON.
#[derive(Debug)]
pub(crate) enum FetchError {
    Url(UrlError),
    Value(String),
}

impl FetchError {
    /// `str(e)`.
    pub(crate) fn message(&self) -> String {
        match self {
            FetchError::Url(e) => e.message(),
            FetchError::Value(m) => m.clone(),
        }
    }

    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            FetchError::Url(e) => e.type_name(),
            FetchError::Value(_) => "JSONDecodeError",
        }
    }
}

fn timeout() -> Duration {
    Duration::from_secs(HTTP_TIMEOUT)
}

/// `http_get_json(url, ca_bundle=...)`.
pub(crate) fn http_get_json(
    url: &str,
    ca_bundle: Option<&str>,
) -> std::result::Result<Value, FetchError> {
    let mut response = urlreq::urlopen(
        &Request {
            url,
            data: None,
            headers: &[],
        },
        timeout(),
        "endpoint",
        ca_bundle,
    )
    .map_err(FetchError::Url)?;
    let body = read_limited(&mut response, url, MAX_RESPONSE_BYTES).map_err(FetchError::Url)?;
    json_loads(&body).map_err(FetchError::Value)
}

/// `urllib.parse.urlencode(form)`.
pub(crate) fn urlencode(form: &[(&str, &str)]) -> String {
    form.iter()
        .map(|(k, v)| format!("{}={}", quote_plus(k), quote_plus(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `urllib.parse.quote_plus(s)`.
pub(crate) fn quote_plus(s: &str) -> String {
    crate::session::url::quote(s, " ").replace(' ', "+")
}

/// `http_post_form(url, form, ca_bundle=..., expect_json=...)`.
pub(crate) fn http_post_form(
    url: &str,
    form: &[(&str, &str)],
    ca_bundle: Option<&str>,
    expect_json: bool,
) -> std::result::Result<Option<Value>, FetchError> {
    let data = urlencode(form);
    let mut response = urlreq::urlopen(
        &Request {
            url,
            data: Some(data.as_bytes()),
            headers: &[("Content-Type", "application/x-www-form-urlencoded")],
        },
        timeout(),
        "endpoint",
        ca_bundle,
    )
    .map_err(FetchError::Url)?;
    if !expect_json {
        return Ok(None);
    }
    let body = read_limited(&mut response, url, MAX_RESPONSE_BYTES).map_err(FetchError::Url)?;
    json_loads(&body).map(Some).map_err(FetchError::Value)
}

/// `require_verified_tls(parsed_globals)`.
pub(crate) fn require_verified_tls(globals: &crate::cli::GlobalArgs) -> Result<()> {
    if globals.no_verify_ssl {
        return Err(Error::Jwt(
            "TLS verification can't be disabled with --authorizer JWT: unlike a SigV4 signature, the bearer token can be replayed by anything that intercepts the connection. If you use a TLS-inspecting proxy, trust its certificate authority with --ca-bundle instead.".to_string(),
        ));
    }
    Ok(())
}

/// `resolve_ca_bundle(session, parsed_globals)`: `--ca-bundle`, else
/// `AWS_CA_BUNDLE` or `ca_bundle` in the config file.
pub(crate) fn resolve_ca_bundle(ctx: &crate::driver::Context) -> Option<String> {
    ctx.globals
        .ca_bundle
        .clone()
        .filter(|c| !c.is_empty())
        .or_else(|| ctx.session.config().get("ca_bundle").ok().flatten())
        .filter(|c| !c.is_empty())
}

/// The model's `RegistryIdentifier` pattern, with the id captured.
static REGISTRY_IDENTIFIER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\A(?:arn:aws(?:-[^:]+)?:agent-registry:[a-z0-9-]+:[0-9]{12}:registry/)?([a-zA-Z0-9]{12,16})\z",
    )
    .expect("regex")
});

/// `bare_registry_id(identifier)`.
pub(crate) fn bare_registry_id(identifier: &str) -> Result<String> {
    match REGISTRY_IDENTIFIER.captures(identifier) {
        Some(c) => Ok(c[1].to_string()),
        None => Err(Error::Jwt(format!(
            "Refusing registry identifier {}: expected a registry id of 12 to 16 letters and digits, or a registry ARN (arn:<partition>:agent-registry:<region>:<account>:registry/<id>).",
            crate::pyfmt::str_repr(identifier)
        ))),
    }
}

/// `_CONFIGURED_ENDPOINT_SETTINGS`.
const CONFIGURED_ENDPOINT_SETTINGS: &str =
    "AWS_ENDPOINT_URL_AGENT_REGISTRY, AWS_ENDPOINT_URL or endpoint_url in the config file";

/// `refuse_endpoint_query(url, parsed_globals)`.
pub(crate) fn refuse_endpoint_query(url: &str, globals: &crate::cli::GlobalArgs) -> Result<()> {
    let parts = urlsplit_parts(url).map_err(|e| Error::Other(e.0))?;
    if parts.query.is_empty() && parts.fragment.is_empty() {
        return Ok(());
    }
    let setting = if globals
        .endpoint_url
        .as_deref()
        .is_some_and(|u| !u.is_empty())
    {
        "--endpoint-url"
    } else {
        CONFIGURED_ENDPOINT_SETTINGS
    };
    Err(Error::Jwt(format!(
        "Refusing registry endpoint {}: it has a query string or fragment, and a registry endpoint can only have a scheme, host and path. Remove the query string or fragment from {setting}.",
        url_origin(url)
    )))
}

/// `_ENDPOINT_PROBE_OPERATION`, `_ENDPOINT_PROBE_REGISTRY_ID`.
const ENDPOINT_PROBE_OPERATION: &str = "ListDiscoverableRegistryRecords";
const ENDPOINT_PROBE_REGISTRY_ID: &str = "endpointprobe";

/// `resolve_data_plane_endpoint(session, parsed_globals, region,
/// https_only)`: the endpoint of a request built (not signed or sent) by
/// an unsigned client of the same session and arguments.
pub(crate) fn resolve_data_plane_endpoint(
    ctx: &crate::driver::Context,
    region: &str,
    https_only: bool,
) -> Result<String> {
    let model = crate::model::ServiceModel::load(super::constants::SERVICE)
        .map_err(|e| Error::Cli(ctx.general_error(&e)))?;
    let mut params = crate::value::Map::new();
    params.insert(
        "registryId".to_string(),
        Value::str(ENDPOINT_PROBE_REGISTRY_ID),
    );
    let (url, url_path) = crate::driver::probe_request_url(
        ctx,
        &model,
        ENDPOINT_PROBE_OPERATION,
        Some(region),
        &Value::Map(params),
    )
    .map_err(|e| Error::Cli(ctx.general_error(&e)))?;
    refuse_endpoint_query(&url, &ctx.globals)?;
    let parts = crate::session::url::urlsplit(&url);
    let path = parts
        .path
        .strip_suffix(url_path.as_str())
        .unwrap_or(&parts.path)
        .to_string();
    let endpoint = crate::session::url::urlunsplit(&crate::session::url::SplitUrl {
        scheme: parts.scheme,
        netloc: parts.netloc,
        path,
        query: String::new(),
        fragment: String::new(),
    });
    if https_only {
        require_https_jwt(&endpoint, "registry endpoint")?;
    }
    Ok(endpoint.trim_end_matches('/').to_string())
}

/// RFC 6749 section 3.3 scope token.
static SCOPE_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A[\x21\x23-\x5b\x5d-\x7e]+\z").expect("regex"));

const MAX_ADVERTISED_SCOPES: usize = 50;
const MAX_SCOPE_LENGTH: usize = 255;

/// `RegistryAuthorization`.
#[derive(Debug, Clone)]
pub(crate) struct RegistryAuthorization {
    pub issuer: String,
    pub scopes: Vec<String>,
}

/// `list(dict.fromkeys(items))`.
pub(crate) fn unique(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for i in items {
        if !out.contains(&i) {
            out.push(i);
        }
    }
    out
}

/// `_advertised_scopes(document, url)`.
fn advertised_scopes(document: &Value, url: &str) -> Vec<String> {
    let scopes = match document.get("scopes_supported") {
        None | Some(Value::Null) => return vec![],
        Some(s) => s,
    };
    let valid = match scopes {
        Value::List(l) if l.len() <= MAX_ADVERTISED_SCOPES => l
            .iter()
            .map(|s| match s {
                Value::Str(s)
                    if s.chars().count() <= MAX_SCOPE_LENGTH && SCOPE_TOKEN.is_match(s) =>
                {
                    Some(s.clone())
                }
                _ => None,
            })
            .collect::<Option<Vec<String>>>(),
        _ => None,
    };
    match valid {
        Some(v) => unique(v),
        None => {
            debug(&format!(
                "Ignoring scopes_supported in {url}: not a list of at most {MAX_ADVERTISED_SCOPES} scope tokens of at most {MAX_SCOPE_LENGTH} characters"
            ));
            vec![]
        }
    }
}

/// `logger.debug` of `awscli.customizations.agentregistry.utils`.
pub(crate) fn debug(message: &str) {
    crate::debuglog::debug("awscli.customizations.agentregistry.utils", message);
}

/// `discover_registry(endpoint, registry_id, ca_bundle)`.
pub(crate) fn discover_registry(
    endpoint: &str,
    registry_id: &str,
    ca_bundle: Option<&str>,
) -> Result<RegistryAuthorization> {
    let url = format!(
        "{endpoint}{}",
        PRM_PATH.replace(
            "{registry_id}",
            &crate::session::url::quote(registry_id, "")
        )
    );
    debug(&format!("Fetching protected resource metadata from {url}"));
    let document = match http_get_json(&url, ca_bundle) {
        Ok(d) => d,
        Err(FetchError::Url(UrlError::Http(e))) => {
            return Err(Error::Jwt(format!(
                "Could not read protected resource metadata for registry {registry_id} (HTTP {}). Confirm the registry id and that the registry uses a CUSTOM_JWT authorizer.",
                e.code
            )));
        }
        Err(FetchError::Url(UrlError::Jwt(m))) => return Err(Error::Jwt(m)),
        Err(e) => {
            return Err(Error::Jwt(format!(
                "Could not read protected resource metadata from {url}: {}",
                printable_or_name(&e.message(), e.type_name())
            )));
        }
    };
    if !matches!(document, Value::Map(_)) {
        return Err(Error::Jwt(format!(
            "The protected resource metadata at {url} is not a JSON object."
        )));
    }
    let servers = match document.get("authorization_servers") {
        Some(v) if v.is_truthy() => v.clone(),
        _ => Value::List(vec![]),
    };
    let Value::List(servers) = servers else {
        return Err(Error::Jwt(format!(
            "The protected resource metadata at {url} lists its authorization_servers in something other than a JSON array."
        )));
    };
    let Some(first) = servers.first() else {
        return Err(Error::Jwt(format!(
            "Registry {registry_id} published no authorization servers, so it is not configured for JWT authorization."
        )));
    };
    let Value::Str(first) = first else {
        return Err(Error::Jwt(format!(
            "The protected resource metadata at {url} names an authorization server that is not a URL string."
        )));
    };
    let issuer = require_https_jwt(first, "authorization server")?;
    Ok(RegistryAuthorization {
        issuer,
        scopes: advertised_scopes(&document, &url),
    })
}

/// `printable(e) or type(e).__name__`.
pub(crate) fn printable_or_name(message: &str, name: &str) -> String {
    let p = printable(message);
    if p.is_empty() { name.to_string() } else { p }
}

/// `same_issuer(first, second)`.
pub(crate) fn same_issuer(first: &str, second: &str) -> bool {
    first.strip_suffix('/').unwrap_or(first) == second.strip_suffix('/').unwrap_or(second)
}

/// `fetch_oidc_metadata(issuer, ca_bundle)`: the discovery document, or
/// an empty map when the issuer has none.
pub(crate) fn fetch_oidc_metadata(issuer: &str, ca_bundle: Option<&str>) -> Result<Value> {
    require_https_jwt(issuer, "issuer")?;
    let url = format!("{}{OIDC_METADATA_PATH}", issuer.trim_end_matches('/'));
    let metadata = match http_get_json(&url, ca_bundle) {
        Ok(m) => m,
        Err(FetchError::Url(UrlError::Http(e))) => {
            if e.code != 404 {
                return Err(Error::Jwt(format!(
                    "Could not read the OIDC discovery document of issuer {issuer}: {}",
                    printable(&format!("HTTP Error {}: {}", e.code, e.reason))
                )));
            }
            debug(&format!("No OIDC discovery document at {url}"));
            return Ok(Value::Map(Default::default()));
        }
        Err(FetchError::Url(UrlError::Url { reason, cert })) => {
            let hint = if cert {
                " Use --ca-bundle to trust a custom certificate authority, such as one that inspects TLS traffic on a corporate network."
            } else {
                ""
            };
            return Err(Error::Jwt(format!(
                "Could not reach the OIDC discovery document of issuer {issuer}: {}.{hint}",
                printable(&reason)
            )));
        }
        Err(FetchError::Url(UrlError::Jwt(m))) => return Err(Error::Jwt(m)),
        Err(e) => {
            return Err(Error::Jwt(format!(
                "Could not read the OIDC discovery document of issuer {issuer}: {}",
                printable_or_name(&e.message(), e.type_name())
            )));
        }
    };
    if !matches!(metadata, Value::Map(_)) {
        return Err(Error::Jwt(format!(
            "The OIDC discovery document at {url} is not a JSON object."
        )));
    }
    for key in ["authorization_endpoint", "token_endpoint"] {
        match metadata.get(key) {
            None | Some(Value::Null) | Some(Value::Str(_)) => {}
            Some(_) => {
                return Err(Error::Jwt(format!(
                    "The OIDC discovery document at {url} has a {key} that is not a URL string."
                )));
            }
        }
    }
    let published = metadata.get("issuer");
    let same = matches!(published, Some(Value::Str(p)) if same_issuer(p, issuer));
    if !same {
        let named = match published {
            Some(Value::Str(p)) => {
                format!("the issuer {}", crate::pyfmt::str_repr(&printable(p)))
            }
            _ => "no issuer".to_string(),
        };
        return Err(Error::Jwt(format!(
            "The OIDC discovery document at {url} names {named}, not {issuer}, which the registry trusts. Refusing to sign in with endpoints published for another issuer."
        )));
    }
    Ok(metadata)
}

/// A string member of a JSON object.
pub(crate) fn str_of<'v>(v: &'v Value, key: &str) -> Option<&'v str> {
    match v.get(key) {
        Some(Value::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// `requested_scopes(metadata)`.
pub(crate) fn requested_scopes(metadata: &Value) -> Vec<String> {
    let supported: Vec<Value> = match metadata.get("scopes_supported") {
        Some(Value::List(l)) => l.clone(),
        _ => vec![],
    };
    if supported.is_empty() {
        return vec!["openid".to_string()];
    }
    let chosen: Vec<String> = WANTED_SCOPES
        .iter()
        .filter(|s| supported.iter().any(|v| v == &Value::str(**s)))
        .map(|s| s.to_string())
        .collect();
    if chosen.is_empty() {
        vec!["openid".to_string()]
    } else {
        chosen
    }
}

/// `with_scopes(defaults, extra)`.
pub(crate) fn with_scopes(defaults: &[String], extra: &[String]) -> Vec<String> {
    unique(defaults.iter().chain(extra).cloned())
}

/// `looks_like_signed_jwt(token)`.
pub(crate) fn looks_like_signed_jwt(token: Option<&Value>) -> bool {
    matches!(token, Some(Value::Str(t)) if t.split('.').count() == 3)
}

/// `select_bearer_token(payload)`: the token and the field it came from.
pub(crate) fn select_bearer_token(payload: &Value) -> Result<(String, &'static str)> {
    if !matches!(payload, Value::Map(_)) {
        return Err(Error::Jwt(
            "The identity provider returned a token response that is not a JSON object."
                .to_string(),
        ));
    }
    let access = payload.get("access_token");
    if looks_like_signed_jwt(access) {
        return Ok((
            str_of(payload, "access_token").unwrap().to_string(),
            "access_token",
        ));
    }
    if looks_like_signed_jwt(payload.get("id_token")) {
        write_status(
            "Note: the provider returned an opaque or encrypted access token, so the signed id_token is being used instead.",
        );
        return Ok((str_of(payload, "id_token").unwrap().to_string(), "id_token"));
    }
    if let Some(Value::Str(a)) = access
        && !a.is_empty()
    {
        return Err(Error::Jwt(format!(
            "The provider returned an access token that is not a signed JWT ({} segments) and no usable id_token, so the registry cannot validate it. The registry owner can configure the identity provider to issue signed JWT access tokens for this client.",
            a.split('.').count()
        )));
    }
    Err(Error::Jwt(
        "The identity provider returned no access token.".to_string(),
    ))
}

/// `redact_logs_and_history(*secrets)`.
pub(crate) fn redact_logs_and_history(secrets: &[&str]) {
    crate::secret_redaction::activate(secrets);
}

/// `note_preset_token(arguments)`.
pub(crate) fn note_preset_token(arguments: &[&str]) {
    let mut unused = vec!["the cached sign-in"];
    unused.extend_from_slice(arguments);
    let (listed, verb) = if unused.len() == 1 {
        (unused[0].to_string(), "is")
    } else {
        (
            format!(
                "{} and {}",
                unused[..unused.len() - 1].join(", "),
                unused[unused.len() - 1]
            ),
            "are",
        )
    };
    write_status(&format!(
        "Using the token in {TOKEN_ENV_VAR}; {listed} {verb} not used."
    ));
}

/// `_SIGNED_IN`: the registry whose cached sign-in this command sends,
/// and whether the hint was printed.
static SIGNED_IN: Mutex<(Option<String>, bool)> = Mutex::new((None, false));

/// `remember_signed_in_registry(registry_id)`.
pub(crate) fn remember_signed_in_registry(registry_id: &str) {
    SIGNED_IN.lock().unwrap().0 = Some(registry_id.to_string());
}

/// `hint_sign_out_on_unauthorized(http_response)`, on `after-call` of the
/// data plane operations.
pub(crate) fn hint_sign_out_on_unauthorized(status: u16) {
    let mut signed_in = SIGNED_IN.lock().unwrap();
    let Some(registry_id) = signed_in.0.clone() else {
        return;
    };
    if signed_in.1 || status != 401 {
        return;
    }
    signed_in.1 = true;
    write_status(&format!(
        "If the registry's sign-in settings changed, run: aws agent-registry sign-out --registry-id {registry_id}"
    ));
}
