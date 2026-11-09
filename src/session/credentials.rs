//! Credential and token resolution (`botocore/credentials.py`
//! `create_credential_resolver`), in botocore's order: the environment
//! (`EnvProvider`, skipped when `--profile` is given), assume role
//! (`AssumeRoleProvider`), then the profile providers
//! (`ProfileProviderBuilder.providers`: web identity, SSO, the shared
//! credentials file, console login, `credential_process`, the config
//! file), the original EC2 credential file, the boto config, the
//! container provider, and IMDS.
//!
//! Assume role, web identity, and SSO credentials are cached in
//! `~/.aws/cli/cache` (`awscli/customizations/assumerole.py` and
//! `awscli/customizations/sso/__init__.py` inject a `JSONFileCache`) and
//! reused until 15 minutes before they expire. Bearer tokens come from
//! the environment for the services that allow it, else from the
//! profile's SSO session (`botocore/tokens.py`).

use std::future::Future;
use std::pin::Pin;

use sha1::{Digest, Sha1};

use super::cache::{self, JsonFileCache};
use super::client::{ApiClient, ApiError};
use super::error::{ErrorKind, SessionError};
use super::http::{HttpClient, HttpConfig, Verify};
use super::pydatetime::{self, PyDateTime};
use super::sign::Credentials;
use super::useragent::{self, Feature};
use crate::config::Config;
use crate::config::ini::{IniValue, Section};
use crate::value::{Map, Value};

/// Credentials with the provider that found them (`credentials.method`),
/// their expiry (`_expiry_time`), and account id.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedCredentials {
    pub credentials: Credentials,
    pub method: &'static str,
    pub expiry: Option<PyDateTime>,
    pub account_id: Option<String>,
}

impl LoadedCredentials {
    fn new(
        method: &'static str,
        access_key: String,
        secret_key: String,
        token: Option<String>,
    ) -> LoadedCredentials {
        LoadedCredentials {
            credentials: Credentials {
                access_key,
                secret_key,
                token,
            },
            method,
            expiry: None,
            account_id: None,
        }
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

fn invalid_config(msg: impl Into<String>) -> SessionError {
    SessionError::new(ErrorKind::InvalidConfig, msg)
}

fn retrieval_error(provider: &str, msg: &str) -> SessionError {
    SessionError::new(
        ErrorKind::CredentialProvider,
        format!("Error when retrieving credentials from {provider}: {msg}"),
    )
}

fn partial(provider: &str, var: &str) -> SessionError {
    SessionError::new(
        ErrorKind::PartialCredentials,
        format!("Partial credentials found in {provider}, missing: {var}"),
    )
}

fn still_expired() -> SessionError {
    SessionError::new(
        ErrorKind::General,
        "Credentials were refreshed, but the refreshed credentials are still expired.",
    )
}

/// `EnvProvider.load`.
pub fn env_credentials(config: &Config) -> Result<Option<Credentials>, SessionError> {
    Ok(env_provider(config)?.map(|c| c.credentials))
}

fn env_provider(config: &Config) -> Result<Option<LoadedCredentials>, SessionError> {
    let get = |k: &str| config.env(k).filter(|v| !v.is_empty()).map(str::to_string);
    let Some(access_key) = get("AWS_ACCESS_KEY_ID") else {
        return Ok(None);
    };
    let Some(secret_key) = get("AWS_SECRET_ACCESS_KEY") else {
        return Err(partial("env", "AWS_SECRET_ACCESS_KEY"));
    };
    let token = ["AWS_SECURITY_TOKEN", "AWS_SESSION_TOKEN"]
        .iter()
        .find_map(|k| get(k));
    let mut c = LoadedCredentials::new("env", access_key, secret_key, token);
    c.account_id = get("AWS_ACCOUNT_ID");
    if let Some(expiry) = get("AWS_CREDENTIAL_EXPIRATION") {
        let parsed = PyDateTime::parse(&expiry).ok_or_else(|| {
            SessionError::new(
                ErrorKind::General,
                format!("Unknown string format: {expiry}"),
            )
        })?;
        // A refresh re-reads the same variables.
        if parsed.seconds_remaining() <= 0.0 {
            return Err(still_expired());
        }
        c.expiry = Some(parsed);
    }
    Ok(Some(c))
}

/// Resolve credentials. `Ok(None)` means none were found (the signer then
/// raises `NoCredentialsError`).
pub async fn resolve_credentials(
    config: &Config,
    region: Option<&str>,
) -> Result<Option<Credentials>, SessionError> {
    Ok(load_credentials(config, region)
        .await?
        .map(|c| c.credentials))
}

/// `Session.get_credentials()`: the first provider of the chain that has
/// credentials.
pub async fn load_credentials(
    config: &Config,
    region: Option<&str>,
) -> Result<Option<LoadedCredentials>, SessionError> {
    // `create_credential_resolver` reads `metadata_service_timeout`, which
    // raises `ProfileNotFound` for a missing profile.
    config.scoped_config()?;
    let profile_name = config.profile().unwrap_or_else(|| "default".to_string());
    let disable_env = config.profile_explicitly_set();
    let resolver = Resolver {
        config,
        region: region.map(str::to_string),
        assume_role_features: std::sync::Mutex::new(Vec::new()),
    };
    if !disable_env && let Some(c) = env_provider(config)? {
        useragent::register(&[Feature::CredentialsEnvVars]);
        return Ok(Some(c));
    }
    let profiles = &config.full_config().profiles;
    let empty = Section::new();
    let profile = profiles.get(&profile_name).unwrap_or(&empty);
    if has_assume_role_config_vars(profile) {
        let mut visited = vec![profile_name.clone()];
        return resolver
            .assume_role(&profile_name, &mut visited)
            .await
            .map(Some);
    }
    if let Some(c) = resolver
        .profile_providers(&profile_name, disable_env)
        .await?
    {
        return Ok(Some(c));
    }
    if let Some(c) = original_ec2_provider(config) {
        return Ok(Some(c));
    }
    if let Some(c) = boto_provider(config)? {
        return Ok(Some(c));
    }
    if let Some(c) = Box::pin(resolver.container()).await? {
        return Ok(Some(c));
    }
    Box::pin(resolver.imds()).await
}

fn has_assume_role_config_vars(profile: &Section) -> bool {
    profile.contains_key("role_arn") && !profile.contains_key("web_identity_token_file")
}

fn has_static_credentials(profile: &Section) -> bool {
    profile.contains_key("aws_secret_access_key") || profile.contains_key("aws_access_key_id")
}

fn section_str(section: &Section, key: &str) -> Option<String> {
    section
        .get(key)
        .and_then(IniValue::as_str)
        .map(str::to_string)
}

struct Resolver<'a> {
    config: &'a Config,
    region: Option<String>,
    /// `AssumeRoleProvider._feature_ids`.
    assume_role_features: std::sync::Mutex<Vec<Feature>>,
}

impl<'a> Resolver<'a> {
    fn profile(&self, name: &str) -> Section {
        self.config
            .full_config()
            .profiles
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    /// `ProfileProviderBuilder.providers(profile_name).load_credentials()`.
    fn profile_providers<'s>(
        &'s self,
        profile_name: &'s str,
        disable_env: bool,
    ) -> BoxFuture<'s, Result<Option<LoadedCredentials>, SessionError>> {
        Box::pin(async move {
            if let Some(c) = self.web_identity(profile_name, disable_env).await? {
                return Ok(Some(c));
            }
            if let Some(c) = self.sso(profile_name).await? {
                return Ok(Some(c));
            }
            if let Some(c) = shared_credentials_provider(self.config, profile_name)? {
                return Ok(Some(c));
            }
            if let Some(c) = self.login(profile_name).await? {
                return Ok(Some(c));
            }
            if let Some(c) = process_provider(self.config, profile_name)? {
                return Ok(Some(c));
            }
            config_file_provider(self.config, profile_name)
        })
    }

    /// `AssumeRoleProvider._load_creds_via_assume_role`.
    fn assume_role<'s>(
        &'s self,
        profile_name: &'s str,
        visited: &'s mut Vec<String>,
    ) -> BoxFuture<'s, Result<LoadedCredentials, SessionError>> {
        Box::pin(async move {
            let profile = self.profile(profile_name);
            let role_arn = section_str(&profile, "role_arn").unwrap_or_default();
            let source_profile = section_str(&profile, "source_profile");
            let credential_source = section_str(&profile, "credential_source");
            match (&credential_source, &source_profile) {
                (Some(_), Some(_)) => {
                    return Err(invalid_config(format!(
                        "The profile \"{profile_name}\" contains both source_profile and credential_source."
                    )));
                }
                (None, None) => {
                    return Err(partial(
                        "assume-role",
                        "source_profile or credential_source",
                    ));
                }
                (Some(source), None) => {
                    if !matches!(
                        source.as_str(),
                        "Environment" | "EcsContainer" | "Ec2InstanceMetadata"
                    ) {
                        return Err(invalid_config(format!(
                            "The credential source \"{source}\" referenced in profile \"{profile_name}\" is not valid."
                        )));
                    }
                }
                (None, Some(source)) => {
                    self.validate_source_profile(profile_name, source, visited)?;
                }
            }
            let source_credentials = match (&credential_source, &source_profile) {
                (Some(source), _) => {
                    self.add_assume_role_feature(Feature::CredentialsProfileNamedProvider);
                    let c = self.credentials_from_source(source, profile_name).await?;
                    self.add_assume_role_feature(match source.as_str() {
                        "Environment" => Feature::CredentialsEnvVars,
                        "EcsContainer" => Feature::CredentialsHttp,
                        _ => Feature::CredentialsImds,
                    });
                    c
                }
                (None, Some(source)) => {
                    visited.push(source.clone());
                    self.add_assume_role_feature(Feature::CredentialsProfileSourceProfile);
                    self.credentials_from_profile(source, visited).await?
                }
                (None, None) => unreachable!(),
            };
            let mut extra = Map::new();
            if let Some(v) = section_str(&profile, "role_session_name") {
                extra.insert("RoleSessionName".into(), Value::str(v));
            }
            if let Some(v) = section_str(&profile, "external_id") {
                extra.insert("ExternalId".into(), Value::str(v));
            }
            let mfa_serial = section_str(&profile, "mfa_serial");
            if let Some(v) = &mfa_serial {
                extra.insert("SerialNumber".into(), Value::str(v));
            }
            if let Some(v) = section_str(&profile, "duration_seconds")
                && let Ok(n) = super::error::py_int(&v)
            {
                extra.insert("DurationSeconds".into(), Value::int(n));
            }
            let fetcher = AssumeRoleFetcher::new(&role_arn, extra);
            let fetcher_features = self.assume_role_features.lock().unwrap().clone();
            self.add_assume_role_feature(Feature::CredentialsStsAssumeRole);
            useragent::register(&self.assume_role_features.lock().unwrap());
            let cache = cache::cli_cache(self.config);
            let response = async {
                Ok::<Value, SessionError>(match fetcher.cached(&cache) {
                    Some(r) => r,
                    None => {
                        let mut kwargs = fetcher.kwargs.clone();
                        if let Some(serial) = &mfa_serial {
                            let code = super::prompt::getpass(&format!(
                                "Enter MFA code for {serial}: "
                            ))
                            .map_err(|e| SessionError::new(ErrorKind::General, e.message()))?;
                            kwargs.insert("TokenCode".into(), Value::str(code));
                        }
                        let client = ApiClient::new(
                            self.config,
                            "sts",
                            self.region.clone(),
                            None,
                            Some(source_credentials.credentials.clone()),
                        )?
                        .with_features(fetcher_features.clone());
                        let mut response = client
                            .call("AssumeRole", &Value::Map(kwargs), None)
                            .await
                            .map_err(ApiError::into_session_error)?;
                        add_account_id_to_response(&mut response);
                        let _ = cache.set(&fetcher.cache_key, &response);
                        response
                    }
                })
            }
            .await
            .map_err(SessionError::into_deferred)?;
            credentials_from_response("assume-role", &response).map_err(SessionError::into_deferred)
        })
    }

    fn add_assume_role_feature(&self, feature: Feature) {
        let mut features = self.assume_role_features.lock().unwrap();
        if !features.contains(&feature) {
            features.push(feature);
        }
    }

    /// `AssumeRoleProvider._validate_source_profile`.
    fn validate_source_profile(
        &self,
        parent: &str,
        source: &str,
        visited: &[String],
    ) -> Result<(), SessionError> {
        let profiles = &self.config.full_config().profiles;
        let Some(source_profile) = profiles.get(source) else {
            return Err(invalid_config(format!(
                "The source_profile \"{source}\" referenced in the profile \"{parent}\" does not exist."
            )));
        };
        if !visited.iter().any(|v| v == source) {
            return Ok(());
        }
        if source != parent || !has_static_credentials(source_profile) {
            return Err(invalid_config(format!(
                "Infinite loop in credential configuration detected. Attempting to load from profile {source} which has already been visited. Visited profiles: [{}]",
                visited
                    .iter()
                    .map(|v| super::error::py_repr_str(v))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(())
    }

    /// `AssumeRoleProvider._resolve_credentials_from_profile`.
    async fn credentials_from_profile(
        &self,
        profile_name: &str,
        visited: &mut Vec<String>,
    ) -> Result<LoadedCredentials, SessionError> {
        let profile = self.profile(profile_name);
        self.add_assume_role_feature(Feature::CredentialsProfile);
        if has_static_credentials(&profile) || !has_assume_role_config_vars(&profile) {
            return match self.profile_providers(profile_name, true).await? {
                Some(c) => Ok(c),
                None => Err(invalid_config(format!(
                    "The source profile \"{profile_name}\" must have credentials."
                ))),
            };
        }
        self.assume_role(profile_name, visited).await
    }

    /// `CanonicalNameCredentialSourcer.source_credentials`.
    async fn credentials_from_source(
        &self,
        source: &str,
        profile_name: &str,
    ) -> Result<LoadedCredentials, SessionError> {
        let creds = match source {
            "Environment" => {
                let c = env_provider(self.config)?;
                if c.is_some() {
                    useragent::register(&[Feature::CredentialsEnvVars]);
                }
                c
            }
            "EcsContainer" => Box::pin(self.container()).await?,
            _ => Box::pin(self.imds()).await?,
        };
        creds.ok_or_else(|| {
            retrieval_error(
                source,
                &format!(
                    "No credentials found in credential_source referenced in profile {profile_name}"
                ),
            )
        })
    }

    /// `AssumeRoleWithWebIdentityProvider.load`.
    async fn web_identity(
        &self,
        profile_name: &str,
        disable_env: bool,
    ) -> Result<Option<LoadedCredentials>, SessionError> {
        let profile = self.profile(profile_name);
        let features = std::sync::Mutex::new(Vec::new());
        let get = |key: &str, env: &str| -> Option<String> {
            let mut features = features.lock().unwrap();
            if !disable_env && let Some(v) = self.config.env(env) {
                if !features.contains(&Feature::CredentialsEnvVarsStsWebIdToken) {
                    features.push(Feature::CredentialsEnvVarsStsWebIdToken);
                }
                return Some(v.to_string());
            }
            let v = section_str(&profile, key);
            if v.is_some() && !features.contains(&Feature::CredentialsProfileStsWebIdToken) {
                features.push(Feature::CredentialsProfileStsWebIdToken);
            }
            v
        };
        let Some(token_path) =
            get("web_identity_token_file", "AWS_WEB_IDENTITY_TOKEN_FILE").filter(|p| !p.is_empty())
        else {
            return Ok(None);
        };
        let Some(role_arn) = get("role_arn", "AWS_ROLE_ARN").filter(|r| !r.is_empty()) else {
            return Err(invalid_config(
                "The provided profile or the current environment is configured to assume role with web identity but has no role ARN configured. Ensure that the profile has the role_arnconfiguration set or the AWS_ROLE_ARN env var is set.",
            ));
        };
        let mut extra = Map::new();
        if let Some(v) = get("role_session_name", "AWS_ROLE_SESSION_NAME") {
            extra.insert("RoleSessionName".into(), Value::str(v));
        }
        let read_token = || {
            std::fs::read_to_string(&token_path).map_err(|e| {
                SessionError::new(
                    ErrorKind::General,
                    crate::cli::paramfile::os_error(&e, &token_path),
                )
            })
        };
        let mut fetcher = AssumeRoleFetcher::new(&role_arn, extra);
        let mut features = features.into_inner().unwrap();
        let fetcher_features = features.clone();
        features.push(Feature::CredentialsStsAssumeRoleWebId);
        useragent::register(&features);
        let mut key_args = fetcher.key_args();
        key_args.insert("WebIdentityToken".into(), Value::str(read_token()?));
        fetcher.cache_key = cache_key_of(&key_args);
        let cache = cache::cli_cache(self.config);
        let response = async {
            Ok::<Value, SessionError>(match fetcher.cached(&cache) {
                Some(r) => r,
                None => {
                    let mut kwargs = fetcher.kwargs.clone();
                    kwargs.insert("WebIdentityToken".into(), Value::str(read_token()?));
                    let client =
                        ApiClient::new(self.config, "sts", self.region.clone(), None, None)?
                            .with_features(fetcher_features.clone());
                    let mut response = client
                        .call("AssumeRoleWithWebIdentity", &Value::Map(kwargs), None)
                        .await
                        .map_err(ApiError::into_session_error)?;
                    add_account_id_to_response(&mut response);
                    let _ = cache.set(&fetcher.cache_key, &response);
                    response
                }
            })
        }
        .await
        .map_err(SessionError::into_deferred)?;
        credentials_from_response("assume-role-with-web-identity", &response)
            .map(Some)
            .map_err(SessionError::into_deferred)
    }

    /// `SSOProvider.load` with `SSOCredentialFetcher`.
    async fn sso(&self, profile_name: &str) -> Result<Option<LoadedCredentials>, SessionError> {
        let full = self.config.full_config();
        let profile = self.profile(profile_name);
        if !profile.contains_key("sso_role_name") && !profile.contains_key("sso_account_id") {
            return Ok(None);
        }
        let mut resolved = profile.clone();
        let mut required: Vec<&str> = vec![
            "sso_role_name",
            "sso_account_id",
            "sso_start_url",
            "sso_region",
        ];
        let session_name = section_str(&profile, "sso_session");
        if let Some(name) = &session_name {
            let Some(session) = full.sso_sessions.get(name) else {
                return Err(invalid_config(format!(
                    "The specified sso-session does not exist: \"{name}\""
                )));
            };
            for (k, v) in session {
                if let Some(existing) = resolved.get(k)
                    && existing != v
                {
                    return Err(invalid_config(format!(
                        "The value for {k} is inconsistent between profile ({}) and sso-session ({}).",
                        ini_str(existing),
                        ini_str(v)
                    )));
                }
                resolved.insert(k.clone(), v.clone());
            }
            required.push("sso_session");
        }
        let missing: Vec<&str> = required
            .iter()
            .copied()
            .filter(|k| !resolved.contains_key(*k))
            .collect();
        if !missing.is_empty() {
            return Err(invalid_config(format!(
                "The profile \"{profile_name}\" is configured to use SSO but is missing required configuration: {}",
                missing.join(", ")
            )));
        }
        let get = |k: &str| section_str(&resolved, k).unwrap_or_default();
        let (start_url, sso_region, role_name, account_id) = (
            get("sso_start_url"),
            get("sso_region"),
            get("sso_role_name"),
            get("sso_account_id"),
        );
        let mut args = Map::new();
        args.insert("accountId".into(), Value::str(&account_id));
        args.insert("roleName".into(), Value::str(&role_name));
        match &session_name {
            Some(name) => args.insert("sessionName".into(), Value::str(name)),
            None => args.insert("startUrl".into(), Value::str(&start_url)),
        };
        let mut sorted: Vec<(String, Value)> = args.into_iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let json = format!(
            "{{{}}}",
            sorted
                .iter()
                .map(|(k, v)| format!(
                    "{}:{}",
                    crate::pyfmt::json_dumps_default(&Value::str(k)),
                    crate::pyfmt::json_dumps_default(v)
                ))
                .collect::<Vec<_>>()
                .join(",")
        );
        let cache_key = make_file_safe(&hex::encode(Sha1::digest(json.as_bytes())));
        let fetcher_features = vec![match &session_name {
            Some(_) => Feature::CredentialsProfileSso,
            None => Feature::CredentialsProfileSsoLegacy,
        }];
        useragent::register(&fetcher_features);
        useragent::register(&[match &session_name {
            Some(_) => Feature::CredentialsSso,
            None => Feature::CredentialsSsoLegacy,
        }]);
        let cache = cache::cli_cache(self.config);
        if let Some(cached) = cached_response(&cache, &cache_key) {
            return credentials_from_response("sso", &cached).map(Some);
        }
        async {
            let token = match &session_name {
                Some(_) => super::sso_token::sso_bearer_token(self.config, profile_name)
                    .await?
                    .unwrap_or_default(),
                None => {
                    let token = super::sso_token::load_token(
                        &cache::sso_cache(self.config),
                        &start_url,
                        None,
                    )?;
                    let expires = token
                        .get("expiresAt")
                        .and_then(Value::as_str)
                        .and_then(PyDateTime::parse);
                    if expires.is_none_or(|e| e.seconds_remaining() <= 0.0) {
                        return Err(unauthorized_sso());
                    }
                    token
                        .get("accessToken")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                }
            };
            let client = ApiClient::new(self.config, "sso", Some(sso_region), None, None)?
                .with_features(fetcher_features);
            let mut params = Map::new();
            params.insert("roleName".into(), Value::str(&role_name));
            params.insert("accountId".into(), Value::str(&account_id));
            params.insert("accessToken".into(), Value::str(token));
            let response = match client
                .call("GetRoleCredentials", &Value::Map(params), None)
                .await
            {
                Ok(r) => r,
                Err(e) if e.code() == Some("UnauthorizedException") => {
                    return Err(unauthorized_sso());
                }
                Err(e) => return Err(e.into_session_error()),
            };
            let rc = response.get("roleCredentials").cloned().unwrap_or_default();
            let s = |k: &str| rc.get(k).cloned().unwrap_or(Value::Null);
            let expiration_ms = rc
                .get("expiration")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let mut creds = Map::new();
            creds.insert("AccessKeyId".into(), s("accessKeyId"));
            creds.insert("SecretAccessKey".into(), s("secretAccessKey"));
            creds.insert("SessionToken".into(), s("sessionToken"));
            creds.insert(
                "Expiration".into(),
                Value::str(pydatetime::utc_from_timestamp(
                    expiration_ms as f64 / 1000.0,
                )),
            );
            creds.insert("AccountId".into(), Value::str(&account_id));
            let mut out = Map::new();
            out.insert("ProviderType".into(), Value::str("sso"));
            out.insert("Credentials".into(), Value::Map(creds));
            let out = Value::Map(out);
            let _ = cache.set(&cache_key, &out);
            credentials_from_response("sso", &out).map(Some)
        }
        .await
        .map_err(SessionError::into_deferred)
    }

    /// `LoginProvider.load`.
    async fn login(&self, profile_name: &str) -> Result<Option<LoadedCredentials>, SessionError> {
        let profile = self.profile(profile_name);
        let Some(session) = section_str(&profile, "login_session") else {
            return Ok(None);
        };
        useragent::register(&super::login_token::FEATURES);
        let c = super::login_token::login_credentials(self.config, &session).await?;
        let mut out = LoadedCredentials::new("login", c.access_key, c.secret_key, Some(c.token));
        out.expiry = PyDateTime::parse(&c.expiry);
        out.account_id = c.account_id;
        Ok(Some(out))
    }

    /// `ContainerProvider.load`.
    async fn container(&self) -> Result<Option<LoadedCredentials>, SessionError> {
        const RELATIVE: &str = "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI";
        const FULL: &str = "AWS_CONTAINER_CREDENTIALS_FULL_URI";
        let full_uri = match (self.config.env(RELATIVE), self.config.env(FULL)) {
            (Some(relative), _) => format!("http://{CONTAINER_IP_ADDRESS}{relative}"),
            (None, Some(full)) => full.to_string(),
            (None, None) => return Ok(None),
        };
        let token = match (
            self.config.env("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE"),
            self.config.env("AWS_CONTAINER_AUTHORIZATION_TOKEN"),
        ) {
            (Some(path), _) => Some(std::fs::read_to_string(path).map_err(|e| {
                SessionError::new(
                    ErrorKind::General,
                    crate::cli::paramfile::os_error(&e, path),
                )
            })?),
            (None, token) => token.map(str::to_string),
        };
        let mut headers = vec![
            ("User-Agent".to_string(), URLLIB3_USER_AGENT.to_string()),
            ("Accept".to_string(), "application/json".to_string()),
        ];
        if let Some(token) = token {
            if token.contains(['\r', '\n']) {
                return Err(SessionError::new(
                    ErrorKind::General,
                    "Auth token value is not a legal header value",
                ));
            }
            headers.push(("Authorization".to_string(), token));
        }
        let response = retrieve_container_credentials(&full_uri, &headers).await?;
        useragent::register(&[Feature::CredentialsHttp]);
        let field = |k: &str| -> Result<Value, SessionError> {
            response
                .get(k)
                .cloned()
                .ok_or_else(|| SessionError::new(ErrorKind::General, format!("'{k}'")))
        };
        let text = |v: Value| v.as_str().map(str::to_string).unwrap_or_default();
        let mut out = LoadedCredentials::new(
            "container-role",
            text(field("AccessKeyId")?),
            text(field("SecretAccessKey")?),
            field("Token")?.as_str().map(str::to_string),
        );
        out.expiry = field("Expiration")?.as_str().and_then(PyDateTime::parse);
        out.account_id = response
            .get("AccountId")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(Some(out))
    }

    /// `InstanceMetadataProvider.load`.
    async fn imds(&self) -> Result<Option<LoadedCredentials>, SessionError> {
        let Some(c) = super::imds::fetch_role_credentials(self.config).await? else {
            return Ok(None);
        };
        useragent::register(&[Feature::CredentialsImds]);
        let mut out = LoadedCredentials::new("iam-role", c.access_key, c.secret_key, Some(c.token));
        out.expiry = PyDateTime::parse(&c.expiry_time);
        Ok(Some(out))
    }
}

/// `ContainerMetadataFetcher.IP_ADDRESS`.
const CONTAINER_IP_ADDRESS: &str = "169.254.170.2";

/// The `User-Agent` urllib3 sends when the request has none
/// (`urllib3.util.request._get_default_user_agent` of the urllib3 the
/// reference bundles).
const URLLIB3_USER_AGENT: &str = "python-urllib3/2.6.3";

/// `ContainerMetadataFetcher.retrieve_full_uri`: the URL must be HTTPS, a
/// loopback address, or an allowed host; three attempts a second apart.
async fn retrieve_container_credentials(
    full_url: &str,
    headers: &[(String, String)],
) -> Result<Value, SessionError> {
    let parsed = url::Url::parse(full_url).ok();
    let hostname = parsed.as_ref().and_then(|u| u.host_str()).map(|h| {
        h.trim_start_matches('[')
            .trim_end_matches(']')
            .to_lowercase()
    });
    let scheme = parsed.as_ref().map(|u| u.scheme().to_string());
    let loopback = hostname
        .as_deref()
        .and_then(|h| h.parse::<std::net::IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback());
    let allowed = [
        "169.254.170.2",
        "169.254.170.23",
        "fd00:ec2::23",
        "localhost",
    ];
    if scheme.as_deref() != Some("https")
        && !loopback
        && !hostname.as_deref().is_some_and(|h| allowed.contains(&h))
    {
        return Err(SessionError::new(
            ErrorKind::General,
            format!(
                "Unsupported host '{}'.  Can only retrieve metadata from a loopback address or one of these hosts: {}",
                hostname.as_deref().unwrap_or("None"),
                allowed.join(", ")
            ),
        ));
    }
    let http = HttpClient::new(HttpConfig {
        verify: Verify::Default,
        connect_timeout: Some(std::time::Duration::from_secs(2)),
        read_timeout: Some(std::time::Duration::from_secs(2)),
        env: std::collections::HashMap::new(),
        proxy_use_forwarding_for_https: false,
    })?;
    let mut attempts = 0;
    loop {
        let message = match http.send("GET", full_url, headers, b"").await {
            Ok(r) => {
                let text = String::from_utf8_lossy(&r.body).into_owned();
                if r.status != 200 {
                    format!(
                        "Received non 200 response {} from container metadata: {text}",
                        r.status
                    )
                } else {
                    match Value::parse_json(&text) {
                        Ok(v) => return Ok(v),
                        Err(_) => "Unable to parse JSON returned from container metadata services"
                            .to_string(),
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind,
                    ErrorKind::ReadTimeout
                        | ErrorKind::Connection
                        | ErrorKind::ConnectionClosed
                        | ErrorKind::ConnectTimeout
                ) =>
            {
                format!(
                    "Received error when attempting to retrieve container metadata: {}",
                    e.message
                )
            }
            Err(e) => return Err(e),
        };
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        attempts += 1;
        if attempts >= 3 {
            return Err(retrieval_error(
                "container-role",
                &format!("Error retrieving metadata: {message}"),
            ));
        }
    }
}

fn ini_str(v: &IniValue) -> String {
    match v {
        IniValue::Str(s) => s.clone(),
        IniValue::Nested(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!(
                    "{}: {}",
                    super::error::py_repr_str(k),
                    super::error::py_repr_str(v)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn unauthorized_sso() -> SessionError {
    SessionError::new(
        ErrorKind::General,
        "The SSO session associated with this profile has expired or is otherwise invalid. To refresh this SSO session run aws sso login with the corresponding profile.",
    )
}

/// `CachedCredentialFetcher._make_file_safe`.
fn make_file_safe(s: &str) -> String {
    s.replace([':', '/'], "_")
}

/// `BaseAssumeRoleCredentialFetcher._create_cache_key` for `args`.
fn cache_key_of(args: &Map) -> String {
    let mut sorted: Vec<(&String, &Value)> = args.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let map: Map = sorted
        .into_iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let text = crate::pyfmt::json_dumps_default(&Value::Map(map));
    make_file_safe(&hex::encode(Sha1::digest(text.as_bytes())))
}

/// `BaseAssumeRoleCredentialFetcher`: the call arguments and cache key.
struct AssumeRoleFetcher {
    kwargs: Map,
    using_default_session_name: bool,
    cache_key: String,
}

impl AssumeRoleFetcher {
    fn new(role_arn: &str, extra: Map) -> AssumeRoleFetcher {
        let mut kwargs = extra;
        kwargs.insert("RoleArn".into(), Value::str(role_arn));
        let mut using_default_session_name = false;
        if kwargs
            .get("RoleSessionName")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            kwargs.insert(
                "RoleSessionName".into(),
                Value::str(format!(
                    "botocore-session-{}",
                    chrono::Utc::now().timestamp()
                )),
            );
            using_default_session_name = true;
        }
        let mut f = AssumeRoleFetcher {
            kwargs,
            using_default_session_name,
            cache_key: String::new(),
        };
        f.cache_key = cache_key_of(&f.key_args());
        f
    }

    fn key_args(&self) -> Map {
        let mut args = self.kwargs.clone();
        if self.using_default_session_name {
            args.shift_remove("RoleSessionName");
        }
        args
    }

    fn cached(&self, cache: &JsonFileCache) -> Option<Value> {
        cached_response(cache, &self.cache_key)
    }
}

/// `CachedCredentialFetcher._load_from_cache`: a cached response that
/// does not expire within 15 minutes.
fn cached_response(cache: &JsonFileCache, key: &str) -> Option<Value> {
    let response = cache.get(key)?;
    let expiration = response
        .get("Credentials")?
        .get("Expiration")?
        .as_str()
        .and_then(PyDateTime::parse)?;
    (expiration.seconds_remaining() >= 15.0 * 60.0).then_some(response)
}

/// `_add_account_id_to_response`.
fn add_account_id_to_response(response: &mut Value) {
    let arn = response
        .get("AssumedRoleUser")
        .and_then(|u| u.get("Arn"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(arn) = arn else {
        return;
    };
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if parts.len() != 6 || parts[0] != "arn" {
        return;
    }
    if let Some(creds) = response
        .as_map_mut()
        .and_then(|m| m.get_mut("Credentials"))
        .and_then(Value::as_map_mut)
    {
        creds.insert("AccountId".into(), Value::str(parts[4]));
    }
}

/// `CachedCredentialFetcher._get_cached_credentials` (and the
/// `RefreshableCredentials` built from it).
fn credentials_from_response(
    method: &'static str,
    response: &Value,
) -> Result<LoadedCredentials, SessionError> {
    let creds = response.get("Credentials").cloned().unwrap_or_default();
    let s = |k: &str| creds.get(k).and_then(Value::as_str).map(str::to_string);
    let mut out = LoadedCredentials::new(
        method,
        s("AccessKeyId").unwrap_or_default(),
        s("SecretAccessKey").unwrap_or_default(),
        s("SessionToken"),
    );
    out.expiry = s("Expiration").and_then(|e| PyDateTime::parse(&e));
    out.account_id = s("AccountId");
    if out.expiry.is_some_and(|e| e.seconds_remaining() <= 0.0) {
        return Err(still_expired());
    }
    Ok(out)
}

/// `SharedCredentialProvider.load`.
fn shared_credentials_provider(
    config: &Config,
    profile_name: &str,
) -> Result<Option<LoadedCredentials>, SessionError> {
    let path = config.credentials_file_path();
    let sections = match crate::config::ini::raw_config_parse(&path) {
        Ok(s) => s,
        Err(crate::config::ini::LoadError::NotFound(_)) => return Ok(None),
        Err(crate::config::ini::LoadError::Parse(p)) => return Err(SessionError::config_parse(&p)),
    };
    let Some(section) = sections.get(profile_name) else {
        return Ok(None);
    };
    let c = static_keys(section, "shared-credentials-file")?;
    if c.is_some() {
        useragent::register(&[Feature::CredentialsProfile]);
    }
    Ok(c)
}

/// `ConfigProvider.load`.
fn config_file_provider(
    config: &Config,
    profile_name: &str,
) -> Result<Option<LoadedCredentials>, SessionError> {
    let path = config.config_file_path();
    let full = match crate::config::ini::load_config(&path) {
        Ok(f) => f,
        Err(crate::config::ini::LoadError::NotFound(_)) => return Ok(None),
        Err(crate::config::ini::LoadError::Parse(p)) => return Err(SessionError::config_parse(&p)),
    };
    let Some(profile) = full.profiles.get(profile_name) else {
        return Ok(None);
    };
    let c = static_keys(profile, "config-file")?;
    if c.is_some() {
        useragent::register(&[Feature::CredentialsProfile]);
    }
    Ok(c)
}

fn static_keys(
    section: &Section,
    method: &'static str,
) -> Result<Option<LoadedCredentials>, SessionError> {
    if !section.contains_key("aws_access_key_id") {
        return Ok(None);
    }
    let get = |k: &str| section_str(section, k);
    let access_key = get("aws_access_key_id").unwrap_or_default();
    let Some(secret_key) = get("aws_secret_access_key") else {
        return Err(partial(method, "aws_secret_access_key"));
    };
    let token = get("aws_security_token").or_else(|| get("aws_session_token"));
    let mut c = LoadedCredentials::new(method, access_key, secret_key, token);
    c.account_id = get("aws_account_id");
    Ok(Some(c))
}

/// `ProcessProvider.load`.
fn process_provider(
    config: &Config,
    profile_name: &str,
) -> Result<Option<LoadedCredentials>, SessionError> {
    let profiles = &config.full_config().profiles;
    let Some(profile) = profiles.get(profile_name) else {
        return Ok(None);
    };
    let Some(command) = section_str(profile, "credential_process") else {
        return Ok(None);
    };
    let profile_account = section_str(profile, "aws_account_id");
    useragent::register(&[Feature::CredentialsProfileProcess]);
    let mut c = run_credential_process(config, &command, profile_account.as_deref())?;
    useragent::register(&[Feature::CredentialsProcess]);
    if let Some(expiry) = c.expiry
        && expiry.seconds_remaining() < 15.0 * 60.0
    {
        // `RefreshableCredentials`: the first access refreshes credentials
        // that expire within the advisory window.
        match run_credential_process(config, &command, profile_account.as_deref()) {
            Ok(fresh) => c = fresh,
            Err(e) if expiry.seconds_remaining() < 10.0 * 60.0 => return Err(e),
            Err(_) => {}
        }
        if c.expiry.is_some_and(|e| e.seconds_remaining() <= 0.0) {
            return Err(still_expired());
        }
    }
    Ok(Some(c))
}

fn run_credential_process(
    config: &Config,
    command: &str,
    profile_account: Option<&str>,
) -> Result<LoadedCredentials, SessionError> {
    let method = "custom-process";
    let args = crate::config::ini::shlex_split(command)
        .ok_or_else(|| SessionError::new(ErrorKind::General, "No closing quotation"))?;
    let Some((program, rest)) = args.split_first() else {
        return Err(SessionError::new(
            ErrorKind::General,
            "list index out of range",
        ));
    };
    let mut cmd = std::process::Command::new(program);
    cmd.args(rest)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    apply_original_ld_library_path(config, &mut cmd);
    for (k, v) in CHILD_ENV.lock().unwrap().iter() {
        cmd.env(k, v);
    }
    let output = cmd.output().map_err(|e| {
        SessionError::new(
            ErrorKind::General,
            crate::cli::paramfile::os_error(&e, program),
        )
    })?;
    if !output.status.success() {
        return Err(retrieval_error(
            method,
            &String::from_utf8_lossy(&output.stderr),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let parsed =
        crate::cli::pyjson::loads(&text).map_err(|e| SessionError::new(ErrorKind::General, e))?;
    let version = parsed.get("Version");
    // `version != 1` with Python equality (`True == 1 == 1.0`).
    let is_one = match version {
        Some(Value::Int(_)) => version.and_then(Value::as_i64) == Some(1),
        Some(Value::Bool(b)) => *b,
        Some(Value::Float(f)) => *f == 1.0,
        _ => false,
    };
    if !is_one {
        let shown = match version {
            Some(v) => crate::pyfmt::py_str(v),
            None => "<Version key not provided>".to_string(),
        };
        return Err(retrieval_error(
            method,
            &format!(
                "Unsupported version '{shown}' for credential process provider, supported versions: 1"
            ),
        ));
    }
    let required = |k: &str| -> Result<String, SessionError> {
        match parsed.get(k) {
            Some(v) => Ok(v
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| crate::pyfmt::py_str(v))),
            None => Err(retrieval_error(
                method,
                &format!("Missing required key in response: '{k}'"),
            )),
        }
    };
    let access_key = required("AccessKeyId")?;
    let secret_key = required("SecretAccessKey")?;
    let opt = |k: &str| {
        parsed
            .get(k)
            .filter(|v| !matches!(v, Value::Null))
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| crate::pyfmt::py_str(v))
            })
    };
    let mut c = LoadedCredentials::new(method, access_key, secret_key, opt("SessionToken"));
    c.account_id = opt("AccountId")
        .filter(|a| !a.is_empty())
        .or_else(|| profile_account.map(str::to_string));
    if let Some(expiry) = opt("Expiration") {
        c.expiry = Some(PyDateTime::parse(&expiry).ok_or_else(|| {
            SessionError::new(
                ErrorKind::General,
                format!("Unknown string format: {expiry}"),
            )
        })?);
    }
    Ok(c)
}

/// Variables set in `os.environ` by the running command
/// (`export-credentials` sets `_AWS_CLI_PROFILE_CHAIN`), passed to child
/// processes.
static CHILD_ENV: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

pub fn set_child_env(name: &str, value: &str) {
    let mut env = CHILD_ENV.lock().unwrap();
    env.retain(|(k, _)| k != name);
    env.push((name.to_string(), value.to_string()));
}

pub fn child_env(name: &str) -> Option<String> {
    CHILD_ENV
        .lock()
        .unwrap()
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

/// `original_ld_library_path`: child processes get the original
/// `LD_LIBRARY_PATH` back from the PyInstaller bootloader.
pub fn apply_original_ld_library_path(config: &Config, cmd: &mut std::process::Command) {
    if config.env("PYINSTALLER_RESET_ENVIRONMENT").is_none() {
        cmd.env("PYINSTALLER_RESET_ENVIRONMENT", "1");
    }
    match config.env("LD_LIBRARY_PATH_ORIG") {
        Some(orig) => {
            cmd.env("LD_LIBRARY_PATH", orig);
        }
        None => {
            cmd.env_remove("LD_LIBRARY_PATH");
        }
    }
}

/// `OriginalEC2Provider.load`.
fn original_ec2_provider(config: &Config) -> Option<LoadedCredentials> {
    let path = config.env("AWS_CREDENTIAL_FILE")?;
    let path = crate::config::ini::expand_path(path, &|_| None);
    let path = crate::cli::paramfile::expanduser(&path);
    let text = std::fs::read_to_string(path).ok()?;
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let access_key = map.get("AWSAccessKeyId")?.clone();
    let secret_key = map.get("AWSSecretKey").cloned().unwrap_or_default();
    Some(LoadedCredentials::new(
        "ec2-credentials-file",
        access_key,
        secret_key,
        None,
    ))
}

/// `BotoProvider.load`.
fn boto_provider(config: &Config) -> Result<Option<LoadedCredentials>, SessionError> {
    let locations: Vec<String> = match config.env("BOTO_CONFIG") {
        Some(p) => vec![p.to_string()],
        None => vec!["/etc/boto.cfg".to_string(), "~/.boto".to_string()],
    };
    for location in locations {
        let path =
            crate::config::ini::expand_path(&location, &|k| config.env(k).map(str::to_string));
        let sections = match crate::config::ini::raw_config_parse(&path) {
            Ok(s) => s,
            Err(crate::config::ini::LoadError::NotFound(_)) => continue,
            Err(crate::config::ini::LoadError::Parse(p)) => {
                return Err(SessionError::config_parse(&p));
            }
        };
        if let Some(section) = sections.get("Credentials")
            && section.contains_key("aws_access_key_id")
        {
            let get = |k: &str| section_str(section, k);
            let Some(secret) = get("aws_secret_access_key") else {
                return Err(partial("boto-config", "aws_secret_access_key"));
            };
            useragent::register(&[Feature::CredentialsBoto2ConfigFile]);
            return Ok(Some(LoadedCredentials::new(
                "boto-config",
                get("aws_access_key_id").unwrap_or_default(),
                secret,
                None,
            )));
        }
    }
    Ok(None)
}

/// A bearer token: `AWS_BEARER_TOKEN_<SIGNING_NAME>` (for the services in
/// `get_bearer_auth_supported_services`), else the profile's SSO token.
pub async fn resolve_token(
    config: &Config,
    signing_name: &str,
    region: Option<&str>,
) -> Result<Option<String>, SessionError> {
    let _ = region;
    if let Some(t) = env_bearer_token(config, signing_name) {
        return Ok(Some(t));
    }
    let profile = config.profile().unwrap_or_else(|| "default".to_string());
    super::sso_token::sso_bearer_token(config, &profile).await
}

/// `get_token_from_environment` for `_should_prefer_bearer_auth`.
pub fn env_bearer_token(config: &Config, signing_name: &str) -> Option<String> {
    if signing_name != "bedrock" {
        return None;
    }
    config
        .env(&format!(
            "AWS_BEARER_TOKEN_{}",
            signing_name.replace(['-', ' '], "_").to_uppercase()
        ))
        .map(str::to_string)
}

/// The errors botocore raises while loading the provider chain at client
/// creation, before the region is checked (`AssumeRoleProvider` validates
/// the role configuration, `AssumeRoleWithWebIdentityProvider` the role
/// ARN, `SSOProvider` the SSO configuration; their network calls are
/// deferred until a request is signed).
pub fn preflight(config: &Config) -> Result<(), SessionError> {
    config.scoped_config()?;
    let profile_name = config.profile().unwrap_or_else(|| "default".to_string());
    let disable_env = config.profile_explicitly_set();
    if !disable_env && env_provider(config)?.is_some() {
        return Ok(());
    }
    let profiles = &config.full_config().profiles;
    let empty = Section::new();
    let profile = profiles.get(&profile_name).unwrap_or(&empty);
    if has_assume_role_config_vars(profile) {
        let mut visited = vec![profile_name.clone()];
        return preflight_assume_role(config, &profile_name, &mut visited);
    }
    preflight_profile(config, &profile_name, disable_env)
}

fn preflight_assume_role(
    config: &Config,
    profile_name: &str,
    visited: &mut Vec<String>,
) -> Result<(), SessionError> {
    let resolver = Resolver {
        config,
        region: None,
        assume_role_features: std::sync::Mutex::new(Vec::new()),
    };
    let profile = resolver.profile(profile_name);
    let source_profile = section_str(&profile, "source_profile");
    let credential_source = section_str(&profile, "credential_source");
    match (&credential_source, &source_profile) {
        (Some(_), Some(_)) => Err(invalid_config(format!(
            "The profile \"{profile_name}\" contains both source_profile and credential_source."
        ))),
        (None, None) => Err(partial(
            "assume-role",
            "source_profile or credential_source",
        )),
        (Some(source), None) => {
            if !matches!(
                source.as_str(),
                "Environment" | "EcsContainer" | "Ec2InstanceMetadata"
            ) {
                return Err(invalid_config(format!(
                    "The credential source \"{source}\" referenced in profile \"{profile_name}\" is not valid."
                )));
            }
            Ok(())
        }
        (None, Some(source)) => {
            resolver.validate_source_profile(profile_name, source, visited)?;
            visited.push(source.clone());
            let source_section = resolver.profile(source);
            if has_static_credentials(&source_section)
                || !has_assume_role_config_vars(&source_section)
            {
                preflight_profile(config, source, true)
            } else {
                preflight_assume_role(config, source, visited)
            }
        }
    }
}

fn preflight_profile(
    config: &Config,
    profile_name: &str,
    disable_env: bool,
) -> Result<(), SessionError> {
    let resolver = Resolver {
        config,
        region: None,
        assume_role_features: std::sync::Mutex::new(Vec::new()),
    };
    let profile = resolver.profile(profile_name);
    let from_env = |env: &str| (!disable_env).then(|| config.env(env)).flatten();
    let token_file = from_env("AWS_WEB_IDENTITY_TOKEN_FILE")
        .map(str::to_string)
        .or_else(|| section_str(&profile, "web_identity_token_file"));
    if token_file.is_some_and(|t| !t.is_empty()) {
        let role = from_env("AWS_ROLE_ARN")
            .map(str::to_string)
            .or_else(|| section_str(&profile, "role_arn"));
        if role.is_none_or(|r| r.is_empty()) {
            return Err(invalid_config(
                "The provided profile or the current environment is configured to assume role with web identity but has no role ARN configured. Ensure that the profile has the role_arnconfiguration set or the AWS_ROLE_ARN env var is set.",
            ));
        }
        return Ok(());
    }
    if profile.contains_key("sso_role_name") || profile.contains_key("sso_account_id") {
        let full = config.full_config();
        let mut resolved = profile.clone();
        let mut required = vec![
            "sso_role_name",
            "sso_account_id",
            "sso_start_url",
            "sso_region",
        ];
        if let Some(name) = section_str(&profile, "sso_session") {
            let Some(session) = full.sso_sessions.get(&name) else {
                return Err(invalid_config(format!(
                    "The specified sso-session does not exist: \"{name}\""
                )));
            };
            for (k, v) in session {
                if let Some(existing) = resolved.get(k)
                    && existing != v
                {
                    return Err(invalid_config(format!(
                        "The value for {k} is inconsistent between profile ({}) and sso-session ({}).",
                        ini_str(existing),
                        ini_str(v)
                    )));
                }
                resolved.insert(k.clone(), v.clone());
            }
            required.push("sso_session");
        }
        let missing: Vec<&str> = required
            .into_iter()
            .filter(|k| !resolved.contains_key(*k))
            .collect();
        if !missing.is_empty() {
            return Err(invalid_config(format!(
                "The profile \"{profile_name}\" is configured to use SSO but is missing required configuration: {}",
                missing.join(", ")
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::GlobalArgs;
    use std::collections::HashMap;

    fn config(env: &[(&str, &str)], profile: Option<&str>) -> Config {
        let mut e: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        e.insert("AWS_CONFIG_FILE".into(), "/nonexistent/config".into());
        e.insert(
            "AWS_SHARED_CREDENTIALS_FILE".into(),
            "/nonexistent/credentials".into(),
        );
        Config::with_env(
            &GlobalArgs {
                profile: profile.map(str::to_string),
                ..Default::default()
            },
            e,
        )
        .unwrap()
    }

    #[test]
    fn env_provider_values() {
        let c = config(
            &[
                ("AWS_ACCESS_KEY_ID", "a"),
                ("AWS_SECRET_ACCESS_KEY", "b"),
                ("AWS_SESSION_TOKEN", "s"),
                ("AWS_SECURITY_TOKEN", "old"),
            ],
            None,
        );
        assert_eq!(
            env_credentials(&c).unwrap(),
            Some(Credentials {
                access_key: "a".into(),
                secret_key: "b".into(),
                token: Some("old".into()),
            })
        );
        let c = config(&[("AWS_ACCESS_KEY_ID", "a")], None);
        assert_eq!(
            env_credentials(&c).unwrap_err().message,
            "Partial credentials found in env, missing: AWS_SECRET_ACCESS_KEY"
        );
    }

    #[test]
    fn assume_role_cache_key() {
        // json.dumps({'DurationSeconds': 3600, 'RoleArn': 'arn:aws:iam::123456789012:role/r'},
        // sort_keys=True) hashed with sha1.
        let mut extra = Map::new();
        extra.insert("DurationSeconds".into(), Value::int(3600));
        let f = AssumeRoleFetcher::new("arn:aws:iam::123456789012:role/r", extra);
        assert_eq!(f.cache_key, "5b4f30aca379fe07625746c18f3f425bb3231f15"); // gitleaks:allow
    }

    #[tokio::test]
    async fn profile_credentials_from_files() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("credentials");
        std::fs::write(
            &creds,
            "[dev]\naws_access_key_id = AKID\naws_secret_access_key = SECRET\n",
        )
        .unwrap();
        let cfg = dir.path().join("config");
        std::fs::write(&cfg, "[profile dev]\nregion = us-east-1\n").unwrap();
        let env: HashMap<String, String> = [
            ("AWS_CONFIG_FILE", cfg.to_str().unwrap()),
            ("AWS_SHARED_CREDENTIALS_FILE", creds.to_str().unwrap()),
            ("AWS_ACCESS_KEY_ID", "ENVKEY"),
            ("AWS_SECRET_ACCESS_KEY", "ENVSECRET"),
            ("AWS_EC2_METADATA_DISABLED", "true"),
            ("HOME", dir.path().to_str().unwrap()),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let c = Config::with_env(
            &GlobalArgs {
                profile: Some("dev".into()),
                ..Default::default()
            },
            env,
        )
        .unwrap();
        let got = load_credentials(&c, Some("us-east-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.credentials.access_key, "AKID");
        assert_eq!(got.method, "shared-credentials-file");
    }
}
