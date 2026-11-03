//! SSO access tokens: the token cache keys (`SSOTokenLoader`,
//! `BaseSSOTokenFetcher._registration_cache_key`) and the bearer token
//! provider with refresh (`botocore/tokens.py` `SSOTokenProvider`).

use sha1::{Digest, Sha1};

use super::cache::{self, JsonFileCache};
use super::client::ApiClient;
use super::error::{ErrorKind, SessionError};
use super::pydatetime::{self, PyDateTime};
use crate::config::Config;
use crate::config::ini::IniValue;
use crate::value::{Map, Value};

/// `SSOTokenLoader._generate_cache_key` / `_token_cache_key`.
pub fn token_cache_key(start_url: &str, session_name: Option<&str>) -> String {
    hex::encode(Sha1::digest(session_name.unwrap_or(start_url).as_bytes()))
}

/// `_registration_cache_key`: `sha1(json.dumps(args, sort_keys=True))`.
pub fn registration_cache_key(
    start_url: &str,
    region: &str,
    scopes: Option<&[String]>,
    session_name: Option<&str>,
) -> String {
    let mut args = Map::new();
    args.insert("region".into(), Value::str(region));
    args.insert(
        "scopes".into(),
        match scopes {
            Some(s) => Value::List(s.iter().map(Value::str).collect()),
            None => Value::Null,
        },
    );
    args.insert(
        "session_name".into(),
        session_name.map_or(Value::Null, Value::str),
    );
    args.insert("startUrl".into(), Value::str(start_url));
    args.insert("tool".into(), Value::str("botocore"));
    let text = crate::pyfmt::json_dumps_default(&Value::Map(args));
    hex::encode(Sha1::digest(text.as_bytes()))
}

fn sso_error(msg: impl Into<String>) -> SessionError {
    SessionError::new(ErrorKind::General, msg)
}

/// `SSOTokenLoader.__call__`.
pub fn load_token(
    cache: &JsonFileCache,
    start_url: &str,
    session_name: Option<&str>,
) -> Result<Value, SessionError> {
    let key = token_cache_key(start_url, session_name);
    let Some(token) = cache.get(&key) else {
        let name = session_name.unwrap_or(start_url);
        return Err(sso_error(format!(
            "Error loading SSO Token: Token for {name} does not exist"
        )));
    };
    if token.get("accessToken").is_none() || token.get("expiresAt").is_none() {
        return Err(sso_error(format!(
            "Error loading SSO Token: Token for {start_url} is invalid"
        )));
    }
    Ok(token)
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The `sso_session` configuration a profile points at
/// (`SSOTokenProvider._load_sso_config`): (session name, region, start
/// URL), `None` when the profile has no `sso_session`.
fn token_provider_config(
    config: &Config,
    profile_name: &str,
) -> Result<Option<(String, String, String)>, SessionError> {
    let full = config.full_config();
    let Some(profile) = full.profiles.get(profile_name) else {
        return Ok(None);
    };
    let Some(session_name) = profile.get("sso_session").and_then(IniValue::as_str) else {
        return Ok(None);
    };
    let Some(sso) = full
        .sso_sessions
        .get(session_name)
        .filter(|s| !s.is_empty())
    else {
        return Err(SessionError::new(
            ErrorKind::InvalidConfig,
            format!(
                "The profile \"{profile_name}\" is configured to use the SSO token provider but the \"{session_name}\" sso_session configuration does not exist."
            ),
        ));
    };
    let missing: Vec<&str> = ["sso_start_url", "sso_region"]
        .into_iter()
        .filter(|k| !sso.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        return Err(SessionError::new(
            ErrorKind::InvalidConfig,
            format!(
                "The profile \"{profile_name}\" is configured to use the SSO token provider but is missing the following configuration: [{}].",
                missing
                    .iter()
                    .map(|m| format!("'{m}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let get = |k: &str| {
        sso.get(k)
            .and_then(IniValue::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Ok(Some((
        session_name.to_string(),
        get("sso_region"),
        get("sso_start_url"),
    )))
}

/// `SSOTokenProvider.load_token().get_frozen_token().token` for the
/// profile; `Ok(None)` when the profile has no `sso_session`.
pub async fn sso_bearer_token(
    config: &Config,
    profile_name: &str,
) -> Result<Option<String>, SessionError> {
    let Some((session_name, region, start_url)) = token_provider_config(config, profile_name)?
    else {
        return Ok(None);
    };
    let cache = cache::sso_cache(config);
    let mut token = load_token(&cache, &start_url, Some(&session_name))?;
    let mut expiration = str_of(&token, "expiresAt")
        .and_then(PyDateTime::parse)
        .ok_or_else(|| sso_error("Unable to parse token expiration"))?;
    if expiration.seconds_remaining() < 15.0 * 60.0
        && let Some(new_token) = refresh_access_token(config, &token, &region, &start_url).await
    {
        token = new_token;
        if let Some(e) = str_of(&token, "expiresAt").and_then(PyDateTime::parse) {
            expiration = e;
        }
        let _ = cache.set(&token_cache_key(&start_url, Some(&session_name)), &token);
    }
    if expiration.seconds_remaining() <= 0.0 {
        return Err(sso_error(
            "Error when retrieving token from sso: Token has expired and refresh failed",
        ));
    }
    Ok(str_of(&token, "accessToken").map(str::to_string))
}

/// `SSOTokenProvider._refresh_access_token`.
async fn refresh_access_token(
    config: &Config,
    token: &Value,
    region: &str,
    start_url: &str,
) -> Option<Value> {
    for k in [
        "refreshToken",
        "clientId",
        "clientSecret",
        "registrationExpiresAt",
    ] {
        token.get(k)?;
    }
    let expiry = str_of(token, "registrationExpiresAt").and_then(PyDateTime::parse)?;
    if expiry.seconds_remaining() <= 0.0 {
        return None;
    }
    let client = ApiClient::new(config, "sso-oidc", Some(region.to_string()), None, None).ok()?;
    let mut params = Map::new();
    params.insert("grantType".into(), Value::str("refresh_token"));
    for k in ["clientId", "clientSecret", "refreshToken"] {
        params.insert(k.into(), token.get(k)?.clone());
    }
    let response = client
        .call("CreateToken", &Value::Map(params), None)
        .await
        .ok()?;
    let expires_in = response.get("expiresIn").and_then(Value::as_i64)?;
    let mut new = Map::new();
    new.insert("startUrl".into(), Value::str(start_url));
    new.insert("region".into(), Value::str(region));
    new.insert("accessToken".into(), response.get("accessToken")?.clone());
    new.insert(
        "expiresAt".into(),
        Value::str(pydatetime::utc_after(expires_in)),
    );
    for k in ["clientId", "clientSecret", "registrationExpiresAt"] {
        new.insert(k.into(), token.get(k)?.clone());
    }
    if let Some(r) = response.get("refreshToken") {
        new.insert("refreshToken".into(), r.clone());
    }
    Some(Value::Map(new))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys() {
        assert_eq!(
            token_cache_key("https://d-1.awsapps.com/start", Some("mysess")),
            hex::encode(Sha1::digest(b"mysess"))
        );
        // json.dumps({'tool': 'botocore', 'startUrl': 'u', 'region': 'r',
        // 'scopes': None, 'session_name': 's'}, sort_keys=True)
        assert_eq!(
            registration_cache_key("u", "r", None, Some("s")),
            hex::encode(Sha1::digest(
                br#"{"region": "r", "scopes": null, "session_name": "s", "startUrl": "u", "tool": "botocore"}"#
            ))
        );
    }
}
