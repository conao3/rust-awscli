//! Credential and token resolution in the order of
//! `botocore/credentials.py` `create_credential_resolver`: the environment
//! (`EnvProvider`, skipped when `--profile` is given), the profile
//! providers (assume role, web identity, SSO, shared credentials, process,
//! config file; `aws-config`'s profile provider), web identity from the
//! environment, the container provider, and IMDS. Bearer tokens come from
//! `aws-config`'s SSO token provider (`botocore/tokens.py`
//! `SSOTokenProvider`).

use aws_config::provider_config::ProviderConfig;
use aws_credential_types::provider::error::CredentialsError;
use aws_credential_types::provider::{ProvideCredentials, token::ProvideToken};
use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};
use aws_types::region::Region;

use super::error::{ErrorKind, SessionError};
use super::sign::Credentials;
use crate::config::Config;

/// `EnvProvider.load`.
pub fn env_credentials(config: &Config) -> Result<Option<Credentials>, SessionError> {
    let access_key = config.env("AWS_ACCESS_KEY_ID").unwrap_or_default();
    if access_key.is_empty() {
        return Ok(None);
    }
    let secret_key = config.env("AWS_SECRET_ACCESS_KEY").unwrap_or_default();
    if secret_key.is_empty() {
        return Err(SessionError::new(
            ErrorKind::PartialCredentials,
            "Partial credentials found in env, missing: AWS_SECRET_ACCESS_KEY",
        ));
    }
    let token = ["AWS_SECURITY_TOKEN", "AWS_SESSION_TOKEN"]
        .iter()
        .find_map(|k| config.env(k))
        .map(str::to_string);
    Ok(Some(Credentials {
        access_key: access_key.to_string(),
        secret_key: secret_key.to_string(),
        token,
    }))
}

fn profile_files(config: &Config) -> EnvConfigFiles {
    EnvConfigFiles::builder()
        .with_file(EnvConfigFileKind::Config, config.config_file_path())
        .with_file(
            EnvConfigFileKind::Credentials,
            config.credentials_file_path(),
        )
        .build()
}

fn provider_config(region: Option<&str>) -> ProviderConfig {
    ProviderConfig::without_region().with_region(region.map(|r| Region::new(r.to_string())))
}

/// Resolve credentials. `Ok(None)` means none were found (the signer then
/// raises `NoCredentialsError`).
pub async fn resolve_credentials(
    config: &Config,
    region: Option<&str>,
) -> Result<Option<Credentials>, SessionError> {
    if !config.profile_explicitly_set()
        && let Some(c) = env_credentials(config)?
    {
        return Ok(Some(c));
    }
    let pc = provider_config(region);
    let profile_name = config.profile().unwrap_or_else(|| "default".to_string());
    // Each provider runs only when the configuration snapshot gives it
    // something to load (the profile's credential keys, or the variables
    // the provider reads), as botocore's providers return `None`
    // otherwise.
    let mut providers: Vec<Box<dyn ProvideCredentials>> = Vec::new();
    let has_profile_credentials = config
        .full_config()
        .profiles
        .get(&profile_name)
        .is_some_and(|p| PROFILE_CREDENTIAL_KEYS.iter().any(|k| p.contains_key(*k)));
    if has_profile_credentials {
        providers.push(Box::new(
            aws_config::profile::ProfileFileCredentialsProvider::builder()
                .configure(&pc)
                .profile_files(profile_files(config))
                .profile_name(&profile_name)
                .build(),
        ));
    }
    if !config.profile_explicitly_set()
        && config.env("AWS_WEB_IDENTITY_TOKEN_FILE").is_some()
        && config.env("AWS_ROLE_ARN").is_some()
    {
        providers.push(Box::new(
            aws_config::web_identity_token::WebIdentityTokenCredentialsProvider::builder()
                .configure(&pc)
                .build(),
        ));
    }
    if config
        .env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
        .is_some()
        || config.env("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some()
    {
        providers.push(Box::new(
            aws_config::ecs::EcsCredentialsProvider::builder()
                .configure(&pc)
                .build(),
        ));
    }
    if config
        .env("AWS_EC2_METADATA_DISABLED")
        .unwrap_or("false")
        .to_lowercase()
        != "true"
    {
        providers.push(Box::new(
            aws_config::imds::credentials::ImdsCredentialsProvider::builder()
                .configure(&pc)
                .build(),
        ));
    }
    for provider in providers {
        match provider.provide_credentials().await {
            Ok(c) => {
                return Ok(Some(Credentials {
                    access_key: c.access_key_id().to_string(),
                    secret_key: c.secret_access_key().to_string(),
                    token: c.session_token().map(str::to_string),
                }));
            }
            Err(CredentialsError::CredentialsNotLoaded(_)) => {}
            Err(e) => {
                return Err(SessionError::new(
                    ErrorKind::CredentialProvider,
                    credentials_error_message(&e),
                ));
            }
        }
    }
    Ok(None)
}

/// The profile keys that make one of botocore's profile providers load
/// credentials (`AssumeRoleProvider`, `AssumeRoleWithWebIdentityProvider`,
/// `SSOProvider`, `SharedCredentialProvider`, `LoginProvider`,
/// `ProcessProvider`, `ConfigProvider`).
const PROFILE_CREDENTIAL_KEYS: &[&str] = &[
    "aws_access_key_id",
    "role_arn",
    "web_identity_token_file",
    "sso_start_url",
    "sso_session",
    "sso_account_id",
    "credential_process",
    "login_session",
];

fn credentials_error_message(e: &CredentialsError) -> String {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        msg = format!("{msg}: {s}");
        source = s.source();
    }
    msg
}

/// A bearer token: `AWS_BEARER_TOKEN_<SIGNING_NAME>` (for the services in
/// `get_bearer_auth_supported_services`), else the profile's SSO token.
pub async fn resolve_token(
    config: &Config,
    signing_name: &str,
    region: Option<&str>,
) -> Result<Option<String>, SessionError> {
    if signing_name == "bedrock" {
        let name = format!(
            "AWS_BEARER_TOKEN_{}",
            signing_name.replace(['-', ' '], "_").to_uppercase()
        );
        if let Some(t) = config.env(&name) {
            return Ok(Some(t.to_string()));
        }
    }
    let provider = aws_config::profile::ProfileFileTokenProvider::builder()
        .profile_files(profile_files(config))
        .profile_name(config.profile().unwrap_or_else(|| "default".to_string()))
        .build();
    let _ = region;
    match provider.provide_token().await {
        Ok(t) => Ok(Some(t.token().to_string())),
        Err(aws_credential_types::provider::error::TokenError::TokenNotLoaded(_)) => Ok(None),
        Err(e) => Err(SessionError::new(
            ErrorKind::CredentialProvider,
            e.to_string(),
        )),
    }
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
    fn env_provider() {
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
        let got = resolve_credentials(&c, Some("us-east-1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.access_key, "AKID");
    }
}
