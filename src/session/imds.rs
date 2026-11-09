//! The EC2 instance metadata service (`botocore/utils.py` `IMDSFetcher`):
//! the region, the last provider of the CLI's region chain
//! (`awscli/utils.py` `IMDSRegionProvider`, `InstanceMetadataRegionFetcher`),
//! and the credentials of the instance's role (`InstanceMetadataFetcher`).

use std::time::Duration;

use crate::config::Config;
use crate::protocol::HttpResponse;
use crate::session::error::{ErrorKind, SessionError};
use crate::session::http::{HttpClient, HttpConfig, Verify};
use crate::value::Value;

const METADATA_BASE_URL: &str = "http://169.254.169.254/";
const METADATA_BASE_URL_IPV6: &str = "http://[fd00:ec2::254]/";

/// `_DEFAULT_ADVISORY_REFRESH_TIMEOUT`, the `ec2_credential_refresh_window`
/// of `create_credential_resolver`.
const CREDENTIAL_REFRESH_WINDOW: i64 = 15 * 60;

/// `urlsplit(url).hostname`: the host of the network location, lower
/// case, without the brackets of an IPv6 address.
fn hostname(url: &str) -> Option<String> {
    let rest = match url.split_once(':') {
        Some((scheme, rest))
            if scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) =>
        {
            rest
        }
        _ => url,
    };
    let netloc = rest.strip_prefix("//")?;
    let netloc = &netloc[..netloc.find(['/', '?', '#']).unwrap_or(netloc.len())];
    let host = netloc.rsplit_once('@').map_or(netloc, |(_, h)| h);
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split_once(']').map_or(v6, |(h, _)| h),
        None => host.split_once(':').map_or(host, |(h, _)| h),
    };
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// `is_valid_uri`: `is_valid_endpoint_url` or `is_valid_ipv6_endpoint_url`.
fn is_valid_uri(url: &str) -> bool {
    if url.contains(['\t', '\r', '\n']) {
        return false;
    }
    let Some(host) = hostname(url) else {
        return false;
    };
    let name = host.strip_suffix('.').unwrap_or(&host);
    let label = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    if host.len() <= 255 && name.split('.').all(label) {
        return true;
    }
    let (address, zone) = match host.split_once('%') {
        Some((a, z)) => (a, Some(z.strip_prefix("25").unwrap_or(z))),
        None => (host.as_str(), None),
    };
    address.parse::<std::net::Ipv6Addr>().is_ok()
        && zone.is_none_or(|z| {
            let z = z.as_bytes();
            let mut i = 0;
            while i < z.len() {
                if z[i] == b'%' {
                    if !(z.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
                        && z.get(i + 2).is_some_and(u8::is_ascii_hexdigit))
                    {
                        return false;
                    }
                    i += 3;
                } else if z[i].is_ascii_alphanumeric() || b"._!-~".contains(&z[i]) {
                    i += 1;
                } else {
                    return false;
                }
            }
            !z.is_empty()
        })
}

/// An `IMDSFetcher` of the session's configuration.
struct Fetcher {
    timeout: Duration,
    attempts: i64,
    base: String,
    disabled: bool,
    v1_disabled: bool,
    env: std::collections::HashMap<String, String>,
}

/// `_fetch_metadata_token`: a token, none, or `BadIMDSRequestError`.
enum Token {
    Token(Option<String>),
    BadRequest,
}

impl Fetcher {
    /// `IMDSFetcher.__init__` (`_select_base_url`): `InvalidIMDSEndpointError`
    /// for a URL `is_valid_uri` rejects, whether or not IMDS is disabled.
    fn new(config: &Config) -> Result<Fetcher, SessionError> {
        let timeout = config
            .get_int("metadata_service_timeout")
            .ok()
            .flatten()
            .unwrap_or(1)
            .max(1) as u64;
        let attempts = config
            .get_int("metadata_service_num_attempts")
            .ok()
            .flatten()
            .unwrap_or(1)
            .max(0);
        let mode = config
            .get("ec2_metadata_service_endpoint_mode")
            .ok()
            .flatten()
            .map(|m| m.to_lowercase())
            .or_else(|| {
                (config.get_bool("imds_use_ipv6").ok().flatten() == Some(true))
                    .then(|| "ipv6".to_string())
            });
        let base = match config.get("ec2_metadata_service_endpoint").ok().flatten() {
            Some(custom) => custom,
            None if mode.as_deref() == Some("ipv6") => METADATA_BASE_URL_IPV6.to_string(),
            None => METADATA_BASE_URL.to_string(),
        };
        if !is_valid_uri(&base) {
            return Err(SessionError::new(
                ErrorKind::General,
                format!("Invalid endpoint EC2 Instance Metadata endpoint: {base}"),
            ));
        }
        Ok(Fetcher {
            timeout: Duration::from_secs(timeout),
            attempts,
            base,
            disabled: config
                .env("AWS_EC2_METADATA_DISABLED")
                .unwrap_or("false")
                .to_lowercase()
                == "true",
            v1_disabled: config.get_bool("ec2_metadata_v1_disabled").ok().flatten() == Some(true),
            env: config.env_map().clone(),
        })
    }

    /// `_construct_url`.
    fn url(&self, path: &str) -> String {
        let sep = if self.base.ends_with('/') { "" } else { "/" };
        format!("{}{sep}{path}", self.base)
    }

    fn http(&self) -> Result<HttpClient, SessionError> {
        HttpClient::new(HttpConfig {
            verify: Verify::Default,
            connect_timeout: Some(self.timeout),
            read_timeout: Some(self.timeout),
            env: self.env.clone(),
            proxy_use_forwarding_for_https: false,
        })
    }

    /// The session's truncated user agent (`session.user_agent(truncate=True)`).
    fn user_agent() -> (String, String) {
        (
            "User-Agent".to_string(),
            format!("aws-cli/{}", crate::model::REFERENCE_VERSION.trim()),
        )
    }

    /// `_fetch_metadata_token`.
    async fn token(&self, http: &HttpClient) -> Result<Token, SessionError> {
        let url = self.url("latest/api/token");
        let headers = [
            (
                "x-aws-ec2-metadata-token-ttl-seconds".to_string(),
                "21600".to_string(),
            ),
            Fetcher::user_agent(),
        ];
        for _ in 0..self.attempts {
            match http.send("PUT", &url, &headers, b"").await {
                Ok(r) if r.status == 200 => {
                    return Ok(Token::Token(Some(
                        String::from_utf8_lossy(&r.body).into_owned(),
                    )));
                }
                Ok(r) if matches!(r.status, 403..=405) => return Ok(Token::Token(None)),
                Ok(r) if r.status == 400 => return Ok(Token::BadRequest),
                Ok(_) => {}
                Err(e) if e.kind == ErrorKind::ReadTimeout => return Ok(Token::Token(None)),
                Err(e)
                    if matches!(
                        e.kind,
                        ErrorKind::Connection
                            | ErrorKind::ConnectionClosed
                            | ErrorKind::ConnectTimeout
                    ) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(Token::Token(None))
    }

    /// `_get_request`: the first response `retry` accepts, or `None` when
    /// the attempts run out (`_RetriesExceededError`).
    async fn get(
        &self,
        http: &HttpClient,
        path: &str,
        token: Option<&str>,
        retry: &(dyn Fn(&HttpResponse) -> bool + Sync),
    ) -> Result<Option<HttpResponse>, SessionError> {
        if token.is_none() && self.v1_disabled {
            return Err(SessionError::new(
                ErrorKind::General,
                "Error retrieving metadata: Unable to retrieve token for use in IMDSv2 call and IMDSv1 has been disabled",
            ));
        }
        let url = self.url(path);
        let mut headers = Vec::new();
        if let Some(t) = token {
            headers.push(("x-aws-ec2-metadata-token".to_string(), t.to_string()));
        }
        headers.push(Fetcher::user_agent());
        for _ in 0..self.attempts {
            match http.send("GET", &url, &headers, b"").await {
                Ok(r) if !retry(&r) => return Ok(Some(r)),
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind,
                        ErrorKind::ReadTimeout
                            | ErrorKind::Connection
                            | ErrorKind::ConnectionClosed
                            | ErrorKind::ConnectTimeout
                    ) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }
}

/// `_default_retry`: a response that is not 200 or has no body.
fn default_retry(r: &HttpResponse) -> bool {
    r.status != 200 || r.body.is_empty()
}

/// The check `create_credential_resolver` makes when it builds the
/// `InstanceMetadataFetcher` of the chain.
pub fn check_endpoint(config: &Config) -> Result<(), SessionError> {
    Fetcher::new(config).map(|_| ())
}

/// `IMDSRegionProvider.provide`: `None` when IMDS is disabled, unreachable,
/// or gives no usable answer.
pub fn fetch_region(config: &Config) -> Result<Option<String>, SessionError> {
    let fetcher = Fetcher::new(config)?;
    if fetcher.disabled {
        return Ok(None);
    }
    let region = async {
        let http = fetcher.http()?;
        let token = match fetcher.token(&http).await? {
            Token::BadRequest => return Ok(None),
            Token::Token(t) => t,
        };
        let path = "latest/meta-data/placement/availability-zone/";
        Ok(fetcher
            .get(&http, path, token.as_deref(), &default_retry)
            .await?
            .map(|r| {
                let mut zone = String::from_utf8_lossy(&r.body).into_owned();
                zone.pop();
                zone
            }))
    };
    std::thread::scope(|s| {
        s.spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| SessionError::new(ErrorKind::General, e.to_string()))?
                .block_on(region)
        })
        .join()
        .unwrap_or(Ok(None))
    })
}

/// The credentials of the instance's IAM role
/// (`InstanceMetadataFetcher.retrieve_iam_role_credentials`).
pub struct RoleCredentials {
    pub access_key: String,
    pub secret_key: String,
    pub token: String,
    pub expiry_time: String,
}

/// `InstanceMetadataFetcher.retrieve_iam_role_credentials` with the
/// session's truncated user agent: `None` when IMDS is disabled, the
/// attempts run out, the request is bad, or the answer lacks a field.
pub async fn fetch_role_credentials(
    config: &Config,
) -> Result<Option<RoleCredentials>, SessionError> {
    let fetcher = Fetcher::new(config)?;
    if fetcher.disabled {
        return Ok(None);
    }
    let http = fetcher.http()?;
    let token = match fetcher.token(&http).await? {
        Token::BadRequest => return Ok(None),
        Token::Token(t) => t,
    };
    let path = "latest/meta-data/iam/security-credentials/";
    let Some(role) = fetcher
        .get(&http, path, token.as_deref(), &default_retry)
        .await?
    else {
        return Ok(None);
    };
    let role_name = String::from_utf8_lossy(&role.body).into_owned();
    let json_retry = |r: &HttpResponse| {
        default_retry(r) || Value::parse_json(&String::from_utf8_lossy(&r.body)).is_err()
    };
    let Some(response) = fetcher
        .get(
            &http,
            &format!("{path}{role_name}"),
            token.as_deref(),
            &json_retry,
        )
        .await?
    else {
        return Ok(None);
    };
    let credentials =
        Value::parse_json(&String::from_utf8_lossy(&response.body)).unwrap_or_default();
    let field = |k: &str| {
        credentials
            .get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    if ["AccessKeyId", "SecretAccessKey", "Token", "Expiration"]
        .iter()
        .any(|k| credentials.get(k).is_none())
    {
        return Ok(None);
    }
    let mut expiry_time = field("Expiration").unwrap_or_default();
    evaluate_expiration(&mut expiry_time);
    Ok(Some(RoleCredentials {
        access_key: field("AccessKeyId").unwrap_or_default(),
        secret_key: field("SecretAccessKey").unwrap_or_default(),
        token: field("Token").unwrap_or_default(),
        expiry_time,
    }))
}

/// `_evaluate_expiration`: credentials that expire within the refresh
/// window and a jitter of 120 to 600 seconds are extended to that much
/// from now, so that a credential service outage does not fail the
/// command.
fn evaluate_expiration(expiry_time: &mut String) {
    let Ok(expiration) = chrono::NaiveDateTime::parse_from_str(expiry_time, "%Y-%m-%dT%H:%M:%SZ")
    else {
        return;
    };
    let interval = CREDENTIAL_REFRESH_WINDOW + rand::random_range(120..=600);
    let now = chrono::Utc::now().naive_utc();
    let offset = chrono::Duration::seconds(interval);
    if now >= expiration - offset {
        *expiry_time = (now + offset).format("%Y-%m-%dT%H:%M:%SZ").to_string();
        crate::debuglog::log(
            "botocore.utils",
            "INFO",
            &format!(
                "Attempting credential expiration extension due to a credential service availability issue. A refresh of these credentials will be attempted again within the next {:.0} minutes.",
                interval as f64 / 60.0
            ),
        );
    }
}
