//! `awscli/customizations/agentregistry/cache.py`: the on-disk cache of
//! registry bearer tokens, one file per endpoint and registry, and the
//! refresh tokens `sign-out` could not revoke.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use super::constants::EXPIRY_SKEW_SECONDS;
use super::utils::{
    self, FetchError, discover_registry, looks_like_signed_jwt, oauth_error_code, printable,
    redact_logs_and_history, same_issuer, select_bearer_token, str_of, write_status,
};
use super::{Error, Result};
use crate::value::{Map, Value};

/// `logger.debug` of `awscli.customizations.agentregistry.cache`.
fn debug(message: &str) {
    crate::debuglog::debug("awscli.customizations.agentregistry.cache", message);
}

/// `CACHE_DIR`.
pub(crate) fn cache_dir() -> String {
    let home = crate::cli::paramfile::expanduser("~");
    format!("{}/.aws/agent-registry/cache", home.trim_end_matches('/'))
}

static ENTRY_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A[0-9a-f]{64}\.json\z").expect("regex"));
const PENDING_REVOCATION: &str = "pending-revocation";
static REGISTRY_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A[a-zA-Z0-9]{12,16}\z").expect("regex"));
static PENDING_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\A(?:([a-zA-Z0-9]{12,16})\.)?[0-9a-f]{64}\.json\z").expect("regex")
});
const PENDING_TOKEN_FIELDS: [&str; 4] =
    ["refreshToken", "revocationEndpoint", "clientId", "issuer"];
static TEMP_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A\.tmp-[^/\\]*\.json\z").expect("regex"));
const STALE_TEMP_SECONDS: f64 = 300.0;
const ERROR_BODY_BYTES: usize = 64 * 1024;
const COGNITO_REJECTIONS: [&str; 3] = [
    "NotAuthorizedException",
    "RefreshTokenReuseException",
    "UserNotFoundException",
];

fn join(dir: &str, name: &str) -> String {
    format!("{dir}/{name}")
}

/// `time.time()`.
pub(crate) fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// `_restrict_permissions(path, mode)`.
fn restrict_permissions(path: &str, mode: u32) {
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        debug(&format!(
            "Could not restrict permissions on {path}: {}",
            crate::cli::paramfile::os_error(&e, path)
        ));
    }
}

/// `_file_name(endpoint, registry_id)`.
fn file_name(endpoint: &str, registry_id: &str) -> String {
    let key = format!("{endpoint}|{registry_id}");
    format!("{}.json", hex::encode(Sha256::digest(key.as_bytes())))
}

/// `entry_path(endpoint, registry_id)`.
pub(crate) fn entry_path(endpoint: &str, registry_id: &str) -> String {
    join(&cache_dir(), &file_name(endpoint, registry_id))
}

/// `pending_revocation_dir()`.
pub(crate) fn pending_revocation_dir() -> String {
    join(&cache_dir(), PENDING_REVOCATION)
}

fn pending_file_name(endpoint: &str, registry_id: &str) -> String {
    let name = file_name(endpoint, registry_id);
    if REGISTRY_ID.is_match(registry_id) {
        format!("{registry_id}.{name}")
    } else {
        name
    }
}

/// `pending_revocation_path(endpoint, registry_id)`.
pub(crate) fn pending_revocation_path(endpoint: &str, registry_id: &str) -> String {
    join(
        &pending_revocation_dir(),
        &pending_file_name(endpoint, registry_id),
    )
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `pending_revocation_registry(path)`.
pub(crate) fn pending_revocation_registry(path: &str) -> Option<String> {
    PENDING_NAME
        .captures(basename(path))
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
}

/// `sorted(os.listdir(directory))`.
fn listdir(directory: &str) -> std::io::Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(directory)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(names)
}

fn temp_files(directory: &str) -> Vec<String> {
    match listdir(directory) {
        Ok(names) => names
            .into_iter()
            .filter(|n| TEMP_NAME.is_match(n))
            .map(|n| join(directory, &n))
            .collect(),
        Err(_) => vec![],
    }
}

/// `list_temp_files()`.
pub(crate) fn list_temp_files() -> Vec<String> {
    let mut files = temp_files(&cache_dir());
    files.extend(temp_files(&pending_revocation_dir()));
    files
}

fn remove_stale_temp_files(directory: &str) {
    let cutoff = now() - STALE_TEMP_SECONDS;
    for path in temp_files(directory) {
        let result = std::fs::symlink_metadata(&path).and_then(|m| {
            let mtime = m
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or_default();
            if mtime < cutoff {
                std::fs::remove_file(&path)?;
            }
            Ok(())
        });
        if let Err(e) = result {
            debug(&format!(
                "Could not remove {path}: {}",
                crate::cli::paramfile::os_error(&e, &path)
            ));
        }
    }
}

/// `os.makedirs(folder, mode=0o700, exist_ok=True)`: intermediate folders
/// with the default mode, the last one `0o700` (less the umask).
fn makedirs(folder: &str) -> std::result::Result<(), String> {
    use std::os::unix::fs::DirBuilderExt as _;
    let path = std::path::Path::new(folder);
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.is_dir()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| crate::cli::paramfile::os_error(&e, &parent.to_string_lossy()))?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(_) if path.is_dir() => Ok(()),
        Err(e) => Err(crate::cli::paramfile::os_error(&e, folder)),
    }
}

/// `tempfile.mkstemp(dir=directory, prefix='.tmp-', suffix='.json')`.
fn mkstemp(directory: &str) -> std::result::Result<(String, std::fs::File), String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
    loop {
        let name: String = (0..8)
            .map(|_| CHARS[rand::random_range(0..CHARS.len())] as char)
            .collect();
        let path = join(directory, &format!(".tmp-{name}.json"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(crate::cli::paramfile::os_error(&e, &path)),
        }
    }
}

/// `_write_entry(path, entry, directory)`: `Err` is `str(OSError)`.
fn write_entry(
    path: &str,
    entry: &Value,
    directory: Option<&str>,
) -> std::result::Result<(), String> {
    let cache = cache_dir();
    let directory = directory
        .map(str::to_string)
        .unwrap_or_else(|| cache.clone());
    let mut folders = vec![cache.clone()];
    if directory != cache {
        folders.push(directory.clone());
    }
    for folder in &folders {
        makedirs(folder)?;
        restrict_permissions(folder, 0o700);
    }
    remove_stale_temp_files(&directory);
    let (temp, mut file) = mkstemp(&directory)?;
    let text = crate::pyfmt::json_dumps_indent2_ascii(entry);
    let result = file
        .write_all(text.as_bytes())
        .and_then(|_| file.flush())
        .map_err(|e| crate::cli::paramfile::os_error(&e, &temp))
        .and_then(|_| {
            drop(file);
            std::fs::rename(&temp, path).map_err(|e| {
                let text = crate::cli::paramfile::os_error(&e, &temp);
                format!("{text} -> {}", crate::pyfmt::str_repr(path))
            })
        });
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// `_read_json(path)`.
fn read_json(path: &str) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    match crate::cli::pyjson::loads(&text) {
        Ok(v @ Value::Map(_)) => Some(v),
        _ => None,
    }
}

/// `read_entry(endpoint, registry_id)`.
pub(crate) fn read_entry(endpoint: &str, registry_id: &str) -> Option<Value> {
    let entry = read_json(&entry_path(endpoint, registry_id))?;
    if str_of(&entry, "endpoint") != Some(endpoint)
        || str_of(&entry, "registryId") != Some(registry_id)
    {
        return None;
    }
    Some(entry)
}

/// `list_entries()`: each entry file, with `None` if unreadable.
pub(crate) fn list_entries() -> Vec<(String, Option<Value>)> {
    let dir = cache_dir();
    match listdir(&dir) {
        Ok(names) => names
            .into_iter()
            .filter(|n| ENTRY_NAME.is_match(n))
            .map(|n| {
                let p = join(&dir, &n);
                let entry = read_json(&p);
                (p, entry)
            })
            .collect(),
        Err(_) => vec![],
    }
}

/// `_pending_tokens(document)`.
fn pending_tokens(document: &Value) -> Option<Vec<Value>> {
    let Some(Value::List(tokens)) = document.get("tokens") else {
        return None;
    };
    if tokens.is_empty() {
        return None;
    }
    for token in tokens {
        let ok = matches!(token, Value::Map(_))
            && token.get("refreshToken").is_some_and(Value::is_truthy)
            && token
                .get("revocationEndpoint")
                .is_some_and(Value::is_truthy)
            && PENDING_TOKEN_FIELDS
                .iter()
                .all(|f| token.get(f).is_none_or(|v| matches!(v, Value::Str(_))));
        if !ok {
            return None;
        }
    }
    Some(tokens.clone())
}

/// A file of pending revocations.
#[derive(Debug, Clone)]
pub(crate) struct Pending {
    pub registry_id: String,
    pub endpoint: String,
    pub tokens: Vec<Value>,
}

/// `read_pending_revocation(path)`: `(document, problem)`.
pub(crate) fn read_pending_revocation(path: &str) -> (Option<Pending>, Option<String>) {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (None, None),
        Err(e) => return (None, Some(crate::cli::paramfile::os_error(&e, path))),
    };
    let document = match String::from_utf8(bytes)
        .ok()
        .and_then(|t| crate::cli::pyjson::loads(&t).ok())
    {
        Some(d) => d,
        None => return (None, Some("it is not valid JSON".to_string())),
    };
    if !matches!(document, Value::Map(_)) {
        return (None, Some("it is not a JSON object".to_string()));
    }
    let (Some(registry_id), Some(endpoint)) = (
        str_of(&document, "registryId").filter(|s| !s.is_empty()),
        str_of(&document, "endpoint").filter(|s| !s.is_empty()),
    ) else {
        return (
            None,
            Some("it names no registry or no endpoint".to_string()),
        );
    };
    if basename(path) != pending_file_name(endpoint, registry_id) {
        return (
            None,
            Some("its name does not match the registry and endpoint in it".to_string()),
        );
    }
    let Some(tokens) = pending_tokens(&document) else {
        return (
            None,
            Some("it holds no list of refresh tokens to revoke".to_string()),
        );
    };
    let refresh: Vec<&str> = tokens
        .iter()
        .filter_map(|t| str_of(t, "refreshToken"))
        .collect();
    redact_logs_and_history(&refresh);
    (
        Some(Pending {
            registry_id: registry_id.to_string(),
            endpoint: endpoint.to_string(),
            tokens: tokens.clone(),
        }),
        None,
    )
}

/// `list_pending_revocations()`: `(path, document, problem)`.
pub(crate) fn list_pending_revocations() -> Vec<(String, Option<Pending>, Option<String>)> {
    let directory = pending_revocation_dir();
    let names = match listdir(&directory) {
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return vec![],
        Err(e) => {
            let problem = crate::cli::paramfile::os_error(&e, &directory);
            return vec![(directory, None, Some(problem))];
        }
    };
    let mut pending = Vec::new();
    for name in names {
        if !PENDING_NAME.is_match(&name) {
            continue;
        }
        let path = join(&directory, &name);
        let (document, problem) = read_pending_revocation(&path);
        if document.is_some() || problem.is_some() {
            pending.push((path, document, problem));
        }
    }
    pending
}

fn pending_value(registry_id: &str, endpoint: &str, tokens: Vec<Value>) -> Value {
    let mut m = Map::new();
    m.insert("registryId".into(), Value::str(registry_id));
    m.insert("endpoint".into(), Value::str(endpoint));
    m.insert("tokens".into(), Value::List(tokens));
    Value::Map(m)
}

/// What `add_pending_revocation` and `remove_pending_revocations` raise:
/// an `OSError` or an `AgentRegistryJWTError`, as `str(e)`.
pub(crate) type CacheError = String;

/// `add_pending_revocation(entry)`.
pub(crate) fn add_pending_revocation(entry: &Value) -> std::result::Result<(), CacheError> {
    let (Some(registry_id), Some(endpoint)) = (
        str_of(entry, "registryId").filter(|s| !s.is_empty()),
        str_of(entry, "endpoint").filter(|s| !s.is_empty()),
    ) else {
        return Err("the cached sign-in names no registry or no endpoint".to_string());
    };
    let mut token = Map::new();
    for field in PENDING_TOKEN_FIELDS {
        if let Some(v) = str_of(entry, field).filter(|s| !s.is_empty()) {
            token.insert(field.to_string(), Value::str(v));
        }
    }
    if !token.contains_key("refreshToken") || !token.contains_key("revocationEndpoint") {
        return Err(
            "the cached sign-in has no refresh token or revocation endpoint that can be kept"
                .to_string(),
        );
    }
    let path = pending_revocation_path(endpoint, registry_id);
    let (document, problem) = read_pending_revocation(&path);
    if let Some(problem) = problem {
        return Err(format!("could not read {path}: {problem}"));
    }
    let mut tokens = document.map(|d| d.tokens).unwrap_or_default();
    let refresh = token.get("refreshToken").cloned();
    if tokens
        .iter()
        .all(|k| k.get("refreshToken") != refresh.as_ref())
    {
        tokens.push(Value::Map(token));
    }
    write_entry(
        &path,
        &pending_value(registry_id, endpoint, tokens),
        Some(&pending_revocation_dir()),
    )
}

/// `remove_pending_revocations(path, revoked)`.
pub(crate) fn remove_pending_revocations(
    path: &str,
    revoked: &[String],
) -> std::result::Result<(), CacheError> {
    let (document, problem) = read_pending_revocation(path);
    if let Some(problem) = problem {
        return Err(format!("could not read {path}: {problem}"));
    }
    let Some(document) = document else {
        return Ok(());
    };
    let left: Vec<Value> = document
        .tokens
        .iter()
        .filter(|t| !str_of(t, "refreshToken").is_some_and(|r| revoked.iter().any(|x| x == r)))
        .cloned()
        .collect();
    if left.is_empty() {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(crate::cli::paramfile::os_error(&e, path));
            }
            _ => {}
        }
    } else if left.len() < document.tokens.len() {
        write_entry(
            path,
            &pending_value(&document.registry_id, &document.endpoint, left),
            Some(&pending_revocation_dir()),
        )?;
    }
    Ok(())
}

/// Python `int(value)` of a JSON value (`None` where it raises).
fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Int(_) => v.as_i64(),
        Value::Float(f) if f.is_finite() => Some(f.trunc() as i64),
        Value::Str(s) => {
            let t = utils::py_strip(s).replace('_', "");
            t.parse::<i64>().ok()
        }
        _ => None,
    }
}

/// `_jwt_exp(token)`: the `exp` claim, unverified.
fn jwt_exp(token: &str) -> Option<i64> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let cleaned: String = payload
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=' | '+' | '/'))
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim_end_matches('=');
    let padded = format!("{cleaned}{}", "=".repeat((4 - cleaned.len() % 4) % 4));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(padded)
        .ok()?;
    let doc = utils::json_loads(&bytes).ok()?;
    py_int(doc.get("exp")?)
}

/// `_lifetime(expires_in)`.
fn lifetime(expires_in: Option<&Value>) -> Option<i64> {
    let v = expires_in?;
    if matches!(v, Value::Bool(_) | Value::Null) {
        return None;
    }
    let seconds = match v {
        Value::Int(_) | Value::Float(_) | Value::Str(_) => py_int(v)?,
        _ => return None,
    };
    (seconds >= 0).then_some(seconds)
}

/// The fields of a sign-in `save_token` writes.
#[derive(Debug, Default, Clone)]
pub(crate) struct Saved<'a> {
    pub token: &'a str,
    pub expires_in: Option<Value>,
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub issuer: Option<String>,
    pub revocation_endpoint: Option<String>,
    pub pool_region: Option<String>,
    pub token_endpoint: Option<String>,
    pub scopes: Vec<String>,
}

/// `save_token(endpoint, registry_id, ...)`.
pub(crate) fn save_token(endpoint: &str, registry_id: &str, saved: &Saved<'_>) {
    let expires_in = lifetime(saved.expires_in.as_ref());
    let mut expires_at = now().floor() as i64 + expires_in.unwrap_or(0);
    if let Some(claimed) = jwt_exp(saved.token).filter(|c| *c != 0) {
        expires_at = if expires_in.is_some_and(|e| e != 0) {
            expires_at.min(claimed)
        } else {
            claimed
        };
    }
    let mut entry = Map::new();
    let mut put = |key: &str, v: Option<&str>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            entry.insert(key.to_string(), Value::str(v));
        }
    };
    put("registryId", Some(registry_id));
    put("endpoint", Some(endpoint));
    put("clientId", saved.client_id.as_deref());
    put("issuer", saved.issuer.as_deref());
    put("accessToken", Some(saved.token));
    if expires_at != 0 {
        entry.insert("expiresAt".to_string(), Value::int(expires_at));
    }
    let mut put = |key: &str, v: Option<&str>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            entry.insert(key.to_string(), Value::str(v));
        }
    };
    put("refreshToken", saved.refresh_token.as_deref());
    put("revocationEndpoint", saved.revocation_endpoint.as_deref());
    put("poolRegion", saved.pool_region.as_deref());
    put("tokenEndpoint", saved.token_endpoint.as_deref());
    if !saved.scopes.is_empty() {
        entry.insert(
            "scopes".to_string(),
            Value::List(saved.scopes.iter().map(Value::str).collect()),
        );
    }
    if let Err(e) = write_entry(&entry_path(endpoint, registry_id), &Value::Map(entry), None) {
        write_status(&format!(
            "Warning: could not save the sign-in for registry {registry_id} to the token cache ({}). A fresh sign-in will be needed next time.",
            printable(&e)
        ));
    }
}

/// `_expires_at(entry)`.
fn expires_at(entry: &Value) -> f64 {
    match entry.get("expiresAt") {
        Some(v @ Value::Int(_)) => v.as_i64().map(|i| i as f64).unwrap_or(f64::MAX),
        Some(Value::Float(f)) => *f,
        _ => 0.0,
    }
}

/// `_entry_scopes(entry)`.
pub(crate) fn entry_scopes(entry: &Value) -> Vec<String> {
    match entry.get("scopes") {
        Some(Value::List(l)) => l
            .iter()
            .filter_map(|s| match s {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

/// `_matches(entry, client_id, scopes)`.
fn matches_entry(entry: &Value, client_id: Option<&str>, scopes: &[String]) -> bool {
    let registry_id = match entry.get("registryId") {
        Some(Value::Str(s)) => s.clone(),
        Some(v) => crate::pyfmt::py_str(v),
        None => "None".to_string(),
    };
    if let Some(c) = client_id.filter(|c| !c.is_empty())
        && Some(c) != str_of(entry, "clientId")
    {
        debug(&format!(
            "Cached sign-in for {registry_id} is for another client id"
        ));
        return false;
    }
    let have = entry_scopes(entry);
    if !scopes.iter().all(|s| have.contains(s)) {
        debug(&format!(
            "Cached sign-in for {registry_id} lacks an advertised scope"
        ));
        return false;
    }
    true
}

/// `_fresh_token(entry)`.
fn fresh_token(entry: &Value) -> Option<String> {
    let token = entry.get("accessToken");
    if looks_like_signed_jwt(token) && expires_at(entry) - EXPIRY_SKEW_SECONDS as f64 > now() {
        return str_of(entry, "accessToken").map(str::to_string);
    }
    None
}

/// `_registry_changed(entry, endpoint, registry_id, ca_bundle)`.
fn registry_changed(
    entry: &Value,
    endpoint: &str,
    registry_id: &str,
    ca_bundle: Option<&str>,
) -> (Option<String>, Vec<String>) {
    let registry = match discover_registry(endpoint, registry_id, ca_bundle) {
        Ok(r) => r,
        Err(e) => {
            debug(&format!(
                "Could not check the issuer and scopes of registry {registry_id} before refreshing: {}",
                printable(&e.message())
            ));
            return (None, vec![]);
        }
    };
    if let Some(issuer) = str_of(entry, "issuer").filter(|s| !s.is_empty())
        && !same_issuer(&registry.issuer, issuer)
    {
        return (
            Some(format!(
                "The registry now trusts {}, not {}",
                printable(&registry.issuer),
                printable(issuer)
            )),
            registry.scopes,
        );
    }
    if !matches_entry(entry, None, &registry.scopes) {
        return (
            Some("The registry now asks for scopes the cached sign-in doesn't have".to_string()),
            registry.scopes,
        );
    }
    (None, registry.scopes)
}

/// `load_token(endpoint, registry_id, client_id, session, ca_bundle)`.
pub(crate) fn load_token(
    ctx: &crate::driver::Context,
    endpoint: &str,
    registry_id: &str,
    client_id: Option<&str>,
    ca_bundle: Option<&str>,
) -> Result<Option<String>> {
    let Some(entry) = read_entry(endpoint, registry_id) else {
        return Ok(None);
    };
    if !matches_entry(&entry, client_id, &[]) {
        return Ok(None);
    }
    if let Some(token) = fresh_token(&entry) {
        debug(&format!(
            "Using cached bearer token for registry {registry_id}"
        ));
        return Ok(Some(token));
    }
    if str_of(&entry, "refreshToken").is_none_or(str::is_empty) {
        return Ok(None);
    }
    let (reason, advertised) = registry_changed(&entry, endpoint, registry_id, ca_bundle);
    if let Some(reason) = reason {
        write_status(&format!("{reason}; signing in again."));
        return Ok(None);
    }
    let Some(latest) = read_entry(endpoint, registry_id) else {
        return Ok(None);
    };
    if !matches_entry(&latest, client_id, &advertised)
        || latest.get("issuer") != entry.get("issuer")
    {
        return Ok(None);
    }
    if refreshed_meanwhile(&entry, &latest)
        && let Some(token) = fresh_token(&latest)
    {
        debug(&format!(
            "Using the sign-in another process refreshed for {registry_id}"
        ));
        return Ok(Some(token));
    }
    let entry = latest;
    let Some(refresh_token) = str_of(&entry, "refreshToken")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return Ok(None);
    };
    debug(&format!(
        "Cached token expired; refreshing for {registry_id}"
    ));
    redact_logs_and_history(&[&refresh_token]);
    let (token, expires_in, rotated) = match refresh(ctx, &entry, ca_bundle) {
        Ok(r) => r,
        Err(e) => {
            if let Some(latest) = read_entry(endpoint, registry_id)
                && matches_entry(&latest, client_id, &advertised)
                && refreshed_meanwhile(&entry, &latest)
                && let Some(token) = fresh_token(&latest)
            {
                debug(&format!(
                    "Using the sign-in another process refreshed for {registry_id}"
                ));
                return Ok(Some(token));
            }
            let (rejected, reason) = e.failure();
            if rejected {
                write_status(&format!(
                    "The identity provider no longer accepts the cached sign-in for registry {registry_id} ({}); signing in again.",
                    printable(&reason)
                ));
                return Ok(None);
            }
            return Err(Error::Jwt(format!(
                "Could not refresh the cached sign-in for registry {registry_id} ({}). Try again; to sign in from scratch instead, run aws agent-registry sign-out --registry-id {registry_id} first.",
                printable(&reason)
            )));
        }
    };
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    let rotated = rotated
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| refresh_token.clone());
    redact_logs_and_history(&[&token, &rotated]);
    let expires_in = match expires_in {
        Some(v) if v.is_truthy() => v,
        _ => Value::int(3600),
    };
    let owned = |k: &str| str_of(&entry, k).map(str::to_string);
    save_token(
        endpoint,
        registry_id,
        &Saved {
            token: &token,
            expires_in: Some(expires_in),
            refresh_token: Some(rotated),
            client_id: owned("clientId"),
            issuer: owned("issuer"),
            revocation_endpoint: owned("revocationEndpoint"),
            pool_region: owned("poolRegion"),
            token_endpoint: owned("tokenEndpoint"),
            scopes: entry_scopes(&entry),
        },
    );
    Ok(Some(token))
}

/// What a refresh raised.
pub(crate) enum RefreshError {
    /// An `HTTPError`, `URLError` or other exception of the OAuth grant.
    Fetch(FetchError),
    /// A `ClientError` of `GetTokensFromRefreshToken`: its code and
    /// `str(error)`.
    Client { code: String, message: String },
    /// Any other exception: `str(e) or type(e).__name__`.
    Other(String),
}

impl RefreshError {
    /// `_refresh_failure(error)`: `(rejected, reason)`.
    fn failure(&self) -> (bool, String) {
        match self {
            RefreshError::Fetch(FetchError::Url(super::urlreq::UrlError::Http(e))) => {
                let body = e.read_error_body(ERROR_BODY_BYTES);
                let rejected = (400..500).contains(&e.code)
                    && oauth_error_code(&body).as_deref() == Some("invalid_grant");
                let detail = if utils::py_strip(&body).is_empty() {
                    String::new()
                } else {
                    format!(": {body}")
                };
                (
                    rejected,
                    format!("HTTP {} from the token endpoint{detail}", e.code),
                )
            }
            RefreshError::Fetch(FetchError::Url(super::urlreq::UrlError::Url {
                reason, ..
            })) => (
                false,
                if reason.is_empty() {
                    "URLError".to_string()
                } else {
                    reason.clone()
                },
            ),
            RefreshError::Fetch(e) => {
                let m = e.message();
                (
                    false,
                    if m.is_empty() {
                        e.type_name().to_string()
                    } else {
                        m
                    },
                )
            }
            RefreshError::Client { code, message } => {
                (COGNITO_REJECTIONS.contains(&code.as_str()), message.clone())
            }
            RefreshError::Other(m) => (false, m.clone()),
        }
    }
}

/// `_refreshed_meanwhile(entry, latest)`.
fn refreshed_meanwhile(entry: &Value, latest: &Value) -> bool {
    latest.get("refreshToken") != entry.get("refreshToken")
        || expires_at(latest) > expires_at(entry)
}

pub(crate) type Refreshed = (Option<String>, Option<Value>, Option<String>);

/// `_refresh(entry, session, ca_bundle)`: `(token, expires_in, rotated)`.
fn refresh(
    ctx: &crate::driver::Context,
    entry: &Value,
    ca_bundle: Option<&str>,
) -> std::result::Result<Refreshed, RefreshError> {
    if entry.get("poolRegion").is_some_and(Value::is_truthy) {
        return super::providers::cognito_refresh(ctx, entry, ca_bundle);
    }
    if let Some(token_endpoint) = entry.get("tokenEndpoint").filter(|v| v.is_truthy()) {
        let Value::Str(token_endpoint) = token_endpoint else {
            return Err(RefreshError::Other(
                "'NoneType' object has no attribute 'startswith'".to_string(),
            ));
        };
        let refresh_token = str_of(entry, "refreshToken").unwrap_or_default();
        let client_id = str_of(entry, "clientId").unwrap_or_default();
        let payload = utils::http_post_form(
            token_endpoint,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", client_id),
            ],
            ca_bundle,
            true,
        )
        .map_err(|e| match e {
            FetchError::Url(super::urlreq::UrlError::Jwt(m)) => RefreshError::Other(m),
            e => RefreshError::Fetch(e),
        })?
        .unwrap_or_default();
        let (token, _) =
            select_bearer_token(&payload).map_err(|e| RefreshError::Other(e.message()))?;
        let rotated = str_of(&payload, "refresh_token").map(str::to_string);
        return Ok((Some(token), payload.get("expires_in").cloned(), rotated));
    }
    Ok((None, None, None))
}
