//! Console login sessions (`aws login`): the token cache
//! (`LoginCredentialsLoader`, `generate_login_cache_key`), the DPoP proof
//! (`build_dpop_header`), the P-256 key handling the AWS CRT does for
//! botocore, and the credential refresh (`LoginCredentialFetcher`).

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use p256::ecdsa::signature::hazmat::PrehashSigner;
use p256::ecdsa::{Signature, SigningKey};
use sha2::{Digest, Sha256};

use super::cache::{self, JsonFileCache};
use super::client::{ApiClient, ApiError};
use super::error::{ErrorKind, SessionError};
use super::pydatetime::{self, PyDateTime};
use super::useragent::Feature;
use crate::config::Config;
use crate::value::{Map, Value};

/// `generate_login_cache_key`.
pub fn cache_key(session_name: &str) -> String {
    hex::encode(Sha256::digest(session_name.as_bytes()))
}

pub fn load_token(cache: &JsonFileCache, session_name: &str) -> Option<Value> {
    cache.get(&cache_key(session_name))
}

pub fn save_token(cache: &JsonFileCache, session_name: &str, token: &Value) {
    let _ = cache.set(&cache_key(session_name), token);
}

/// A P-256 key pair (`awscrt.crypto.EC`).
pub struct DpopKey(SigningKey);

impl DpopKey {
    /// `EC.new_generate(ECType.P_256)`.
    pub fn generate() -> DpopKey {
        let mut bytes = [0u8; 32];
        loop {
            rand::fill(&mut bytes[..]);
            if let Ok(k) = SigningKey::from_slice(&bytes) {
                return DpopKey(k);
            }
        }
    }

    /// `EC.new_key_from_der_data` (SEC1 `ECPrivateKey` or PKCS #8).
    pub fn from_der(der: &[u8]) -> Option<DpopKey> {
        use p256::pkcs8::DecodePrivateKey;
        if let Ok(sk) = p256::SecretKey::from_sec1_der(der) {
            return Some(DpopKey(SigningKey::from(sk)));
        }
        SigningKey::from_pkcs8_der(der).ok().map(DpopKey)
    }

    /// `export_key(ECExportFormat.SEC1)`: the RFC 5915 `ECPrivateKey` with
    /// the curve parameters and the public key, as the AWS CRT writes it.
    pub fn to_sec1_der(&self) -> Vec<u8> {
        let mut der = vec![0x30, 0x77, 0x02, 0x01, 0x01, 0x04, 0x20];
        der.extend_from_slice(&self.0.to_bytes());
        der.extend_from_slice(&[
            0xa0, 0x0a, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0xa1, 0x44,
            0x03, 0x42, 0x00,
        ]);
        der.extend_from_slice(self.0.verifying_key().to_sec1_point(false).as_bytes());
        der
    }

    /// `_serialize_to_pem`: the SEC1 DER, base64, wrapped at 64 columns.
    pub fn to_pem(&self) -> String {
        let b64 = STANDARD.encode(self.to_sec1_der());
        let lines: Vec<&str> = b64
            .as_bytes()
            .chunks(64)
            .map(|c| std::str::from_utf8(c).unwrap_or_default())
            .collect();
        format!(
            "-----BEGIN EC PRIVATE KEY-----\n{}\n-----END EC PRIVATE KEY-----\n",
            lines.join("\n")
        )
    }

    /// `LoginCredentialFetcher._load_private_key`.
    pub fn from_pem(pem: &str) -> Result<DpopKey, String> {
        let contents: String = pem
            .lines()
            .filter(|l| !l.starts_with("-----BEGIN") && !l.starts_with("-----END"))
            .collect::<Vec<_>>()
            .join("");
        let der = STANDARD
            .decode(contents.trim())
            .map_err(|e| e.to_string())?;
        DpopKey::from_der(&der).ok_or_else(|| "invalid key".to_string())
    }

    /// `build_dpop_header(private_key, uri)`.
    pub fn dpop_header(&self, uri: &str) -> String {
        let point = self.0.verifying_key().to_sec1_point(false);
        let x = URL_SAFE_NO_PAD.encode(point.x().expect("x"));
        let y = URL_SAFE_NO_PAD.encode(point.y().expect("y"));
        let header = format!(
            r#"{{"typ":"dpop+jwt","alg":"ES256","jwk":{{"kty":"EC","x":"{x}","y":"{y}","crv":"P-256"}}}}"#
        );
        let iat = chrono::Utc::now().timestamp();
        let jti = uuid::Uuid::new_v4();
        let payload = format!(
            r#"{{"htm":"POST","htu":{},"iat":{iat},"jti":"{jti}"}}"#,
            crate::pyfmt::json_dumps_default(&Value::str(uri))
        );
        let header_b64 = URL_SAFE_NO_PAD.encode(header.as_bytes());
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload.as_bytes());
        let signing_input = format!("{header_b64}.{payload_b64}");
        let digest = Sha256::digest(signing_input.as_bytes());
        let sig: Signature = self.0.sign_prehash(&digest).expect("ECDSA signature");
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
        format!("{signing_input}.{sig_b64}")
    }
}

/// `LoginProvider._feature_ids`.
pub const FEATURES: [Feature; 2] = [Feature::CredentialsProfileLogin, Feature::CredentialsLogin];

fn login_error(msg: impl Into<String>) -> SessionError {
    SessionError::new(ErrorKind::General, msg)
}

/// Credentials from a login token: (access key, secret key, session
/// token, expiry, account id).
pub struct LoginCredentials {
    pub access_key: String,
    pub secret_key: String,
    pub token: String,
    pub expiry: String,
    pub account_id: Option<String>,
}

fn token_to_credentials(token: &Value) -> Option<LoginCredentials> {
    let at = token.get("accessToken")?;
    let s = |k: &str| at.get(k).and_then(Value::as_str).map(str::to_string);
    Some(LoginCredentials {
        access_key: s("accessKeyId")?,
        secret_key: s("secretAccessKey")?,
        token: s("sessionToken")?,
        expiry: s("expiresAt")?,
        account_id: s("accountId"),
    })
}

/// `LoginCredentialFetcher.load_cached_credentials`: the credentials of the
/// cached token, which `LoginProvider.load` returns before any refresh.
pub fn cached_login_credentials(
    config: &Config,
    session_name: &str,
) -> Result<LoginCredentials, SessionError> {
    let cache = cache::login_cache(config);
    let Some(token) = load_token(&cache, session_name) else {
        return Err(login_error(format!(
            "Error loading login session token: Unable to load a existing login session for session {session_name}, Please reauthenticate with 'aws login'."
        )));
    };
    let complete = ["accessToken", "refreshToken", "dpopKey", "clientId"]
        .iter()
        .all(|k| token.get(k).is_some());
    let cached = if complete {
        token_to_credentials(&token)
    } else {
        None
    };
    let Some(cached) = cached else {
        return Err(login_error(
            "Error loading login session token: Failed to load access token from token cache, missing one or more required fields.",
        ));
    };
    Ok(cached)
}

/// `LoginProvider.load` + `RefreshableCredentials` refresh: the cached
/// credentials, refreshed with `signin:CreateOAuth2Token` when they expire
/// within the refresh windows.
pub async fn login_credentials(
    config: &Config,
    session_name: &str,
) -> Result<LoginCredentials, SessionError> {
    let cache = cache::login_cache(config);
    let cached = cached_login_credentials(config, session_name)?;
    // `RefreshableCredentials`: advisory refresh 15 minutes before
    // expiry, mandatory 10 minutes before.
    let remaining = PyDateTime::parse(&cached.expiry)
        .map(|e| e.seconds_remaining())
        .unwrap_or(0.0);
    if remaining >= 15.0 * 60.0 {
        return Ok(cached);
    }
    match refresh(config, &cache, session_name).await {
        Ok(c) => Ok(c),
        Err(_) if remaining >= 10.0 * 60.0 => Ok(cached),
        Err(e) => Err(e.into_deferred()),
    }
}

/// `LoginCredentialFetcher.refresh_credentials`.
async fn refresh(
    config: &Config,
    cache: &JsonFileCache,
    session_name: &str,
) -> Result<LoginCredentials, SessionError> {
    FEATURES
        .iter()
        .copied()
        .for_each(super::useragent::register_context);
    let Some(mut token) = load_token(cache, session_name) else {
        return Err(login_error(
            "Error loading login session token: Private key not found in cached token.",
        ));
    };
    let Some(pem) = token.get("dpopKey").and_then(Value::as_str) else {
        return Err(login_error(
            "Error loading login session token: Private key not found in cached token.",
        ));
    };
    let key = DpopKey::from_pem(pem).map_err(|e| {
        login_error(format!(
            "Error loading login session token: Unable to load private key from cached token: {e}"
        ))
    })?;
    if let Some(exp) = token
        .get("accessToken")
        .and_then(|a| a.get("expiresAt"))
        .and_then(Value::as_str)
        .and_then(PyDateTime::parse)
        && exp.seconds_remaining() > 5.0 * 60.0
        && let Some(c) = token_to_credentials(&token)
    {
        return Ok(c);
    }
    let region = token
        .get("signInRegion")
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .or_else(|| config.region().ok().flatten());
    let client =
        ApiClient::new(config, "signin", region, None, None)?.with_features(FEATURES.to_vec());
    let mut input = Map::new();
    input.insert(
        "clientId".into(),
        token.get("clientId").cloned().unwrap_or(Value::Null),
    );
    input.insert(
        "refreshToken".into(),
        token.get("refreshToken").cloned().unwrap_or(Value::Null),
    );
    input.insert("grantType".into(), Value::str("refresh_token"));
    let mut params = Map::new();
    params.insert("tokenInput".into(), Value::Map(input));
    let dpop = |url: &str| vec![("DPoP".to_string(), key.dpop_header(url))];
    let response = match client
        .call("CreateOAuth2Token", &Value::Map(params), Some(&dpop))
        .await
    {
        Ok(r) => r,
        Err(ApiError::Service(f)) if f.code == "AccessDeniedException" => {
            let error_type = f.parsed.get("error").and_then(Value::as_str).unwrap_or("");
            return Err(login_error(match error_type {
                "TOKEN_EXPIRED" => {
                    "Your session has expired. Please reauthenticate using 'aws login'."
                }
                "USER_CREDENTIALS_CHANGED" => {
                    "Unable to refresh login credentials because of a change in your password. Please reauthenticate with your new password using 'aws login'."
                }
                "INSUFFICIENT_PERMISSIONS" => {
                    "Unable to create or refresh login credentials due to insufficient permissions. You may be missing permission for the 'signin:CreateOAuth2Token' action."
                }
                _ => {
                    "An unspecified error happened when resolving AWS credentials or refreshing a login session profile."
                }
            }));
        }
        Err(e) => return Err(e.into_session_error()),
    };
    let Some(output) = response.get("tokenOutput") else {
        return Err(login_error(
            "Error loading login session token: Failed to refresh an access token.",
        ));
    };
    let expires_in = output.get("expiresIn").and_then(Value::as_i64).unwrap_or(0);
    let at = output.get("accessToken").cloned().unwrap_or(Value::Null);
    let account_id = token
        .get("accessToken")
        .and_then(|a| a.get("accountId"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut new_at = Map::new();
    for (k, src) in [
        ("accessKeyId", "accessKeyId"),
        ("secretAccessKey", "secretAccessKey"),
        ("sessionToken", "sessionToken"),
    ] {
        new_at.insert(k.into(), at.get(src).cloned().unwrap_or(Value::Null));
    }
    new_at.insert("accountId".into(), account_id);
    new_at.insert(
        "expiresAt".into(),
        Value::str(pydatetime::utc_after(expires_in)),
    );
    if let Some(m) = token.as_map_mut() {
        m.insert("accessToken".into(), Value::Map(new_at));
        m.insert(
            "refreshToken".into(),
            output.get("refreshToken").cloned().unwrap_or(Value::Null),
        );
    }
    save_token(cache, session_name, &token);
    token_to_credentials(&token).ok_or_else(|| {
        login_error("Error loading login session token: Failed to refresh an access token.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pem_round_trip_and_dpop() {
        let key = DpopKey::generate();
        let pem = key.to_pem();
        assert!(pem.starts_with("-----BEGIN EC PRIVATE KEY-----\n"));
        let again = DpopKey::from_pem(&pem).unwrap();
        assert_eq!(again.to_pem(), pem);
        let jwt = key.dpop_header("https://signin.aws/v1/token");
        assert_eq!(jwt.split('.').count(), 3);
    }

    #[test]
    fn sec1_matches_aws_crt() {
        // `EC.new_generate(ECType.P_256).export_key(ECExportFormat.SEC1)`.
        let crt = "MHcCAQEEIImoGl2SaIn8xx7pkP2gqLASsLDp0NIc0IG09EP4AsOYoAoGCCqGSM49AwEHoUQDQgAEdp2R9pO6cl3rjSS6TW3YrJV5Wl9W4JrI2EpW7lutrQECI0MQ3hxwN61+h8nHkber6qFaYJ5276OaH3hkvGGjPA==";
        let der = STANDARD.decode(crt).unwrap();
        assert_eq!(DpopKey::from_der(&der).unwrap().to_sec1_der(), der);
    }
}
