//! The region from the EC2 instance metadata service, the last provider
//! of the CLI's region chain (`awscli/utils.py` `IMDSRegionProvider`,
//! `botocore/utils.py` `IMDSFetcher`, `InstanceMetadataRegionFetcher`).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::config::Config;
use crate::session::error::{ErrorKind, SessionError};
use crate::session::http::{HttpClient, HttpConfig, Verify};
use crate::value::Value;

const METADATA_BASE_URL: &str = "http://169.254.169.254/";
const METADATA_BASE_URL_IPV6: &str = "http://[fd00:ec2::254]/";

/// The configuration of `IMDSFetcher`: `None` when IMDS is disabled
/// (`AWS_EC2_METADATA_DISABLED=true`).
struct Settings {
    timeout: Duration,
    attempts: i64,
    base: String,
    v1_disabled: bool,
}

impl Settings {
    fn new(config: &Config) -> Option<Settings> {
        if config
            .env("AWS_EC2_METADATA_DISABLED")
            .unwrap_or("false")
            .to_lowercase()
            == "true"
        {
            return None;
        }
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
        Some(Settings {
            timeout: Duration::from_secs(timeout),
            attempts,
            base,
            v1_disabled: config.get_bool("ec2_metadata_v1_disabled").ok().flatten() == Some(true),
        })
    }

    /// `_construct_url`.
    fn url(&self, path: &str) -> String {
        let sep = if self.base.ends_with('/') { "" } else { "/" };
        format!("{}{sep}{path}", self.base)
    }
}

/// `InstanceMetadataRegionFetcher.retrieve_region`: `None` when IMDS is
/// disabled, unreachable, or gives no usable answer.
pub fn fetch_region(config: &Config) -> Option<String> {
    let settings = Settings::new(config)?;
    let (timeout, attempts, v1_disabled) =
        (settings.timeout, settings.attempts, settings.v1_disabled);
    let url = |path: &str| settings.url(path);

    let mut token = None;
    for _ in 0..attempts {
        match request(
            "PUT",
            &url("latest/api/token"),
            &[("x-aws-ec2-metadata-token-ttl-seconds", "21600")],
            timeout,
        ) {
            Ok((200, body)) => {
                token = Some(body);
                break;
            }
            Ok((403..=405, _)) => break,
            Ok((400, _)) => return None,
            Ok(_) => {}
            Err(true) => break,
            Err(false) => {}
        }
    }
    if token.is_none() && v1_disabled {
        return None;
    }
    let mut headers = Vec::new();
    if let Some(t) = &token {
        headers.push(("x-aws-ec2-metadata-token", t.as_str()));
    }
    for _ in 0..attempts {
        if let Ok((200, body)) = request(
            "GET",
            &url("latest/meta-data/placement/availability-zone/"),
            &headers,
            timeout,
        ) && !body.is_empty()
        {
            let mut region = body;
            region.pop();
            return Some(region);
        }
    }
    None
}

/// The credentials of the instance's IAM role
/// (`InstanceMetadataFetcher.retrieve_iam_role_credentials`).
pub struct RoleCredentials {
    pub access_key: String,
    pub secret_key: String,
    pub token: String,
    pub expiry_time: String,
}

/// An `IMDSFetcher` request: `Err(None)` for an error botocore retries
/// (`RETRYABLE_HTTP_ERRORS`).
async fn send(
    http: &HttpClient,
    method: &str,
    url: &str,
    headers: &[(String, String)],
) -> Result<crate::protocol::HttpResponse, Option<SessionError>> {
    http.send(method, url, headers, b"")
        .await
        .map_err(|e| match e.kind {
            ErrorKind::ReadTimeout
            | ErrorKind::Connection
            | ErrorKind::ConnectionClosed
            | ErrorKind::ConnectTimeout => None,
            _ => Some(e),
        })
}

/// `InstanceMetadataFetcher.retrieve_iam_role_credentials` with the
/// session's truncated user agent: `None` when IMDS is disabled, the
/// attempts run out, the request is bad, or the answer lacks a field.
pub async fn fetch_role_credentials(
    config: &Config,
) -> Result<Option<RoleCredentials>, SessionError> {
    let Some(settings) = Settings::new(config) else {
        return Ok(None);
    };
    let http = HttpClient::new(HttpConfig {
        verify: Verify::Default,
        connect_timeout: Some(settings.timeout),
        read_timeout: Some(settings.timeout),
        env: config.env_map().clone(),
        proxy_use_forwarding_for_https: false,
    })?;
    let user_agent = (
        "User-Agent".to_string(),
        format!("aws-cli/{}", crate::model::REFERENCE_VERSION.trim()),
    );
    let mut token = None;
    let token_url = settings.url("latest/api/token");
    let headers = [
        (
            "x-aws-ec2-metadata-token-ttl-seconds".to_string(),
            "21600".to_string(),
        ),
        user_agent.clone(),
    ];
    for _ in 0..settings.attempts {
        match http.send("PUT", &token_url, &headers, b"").await {
            Ok(r) if r.status == 200 => {
                token = Some(String::from_utf8_lossy(&r.body).into_owned());
                break;
            }
            Ok(r) if matches!(r.status, 403..=405) => break,
            Ok(r) if r.status == 400 => return Ok(None),
            Ok(_) => {}
            Err(e) if e.kind == ErrorKind::ReadTimeout => break,
            Err(e)
                if matches!(
                    e.kind,
                    ErrorKind::Connection | ErrorKind::ConnectionClosed | ErrorKind::ConnectTimeout
                ) => {}
            Err(e) => return Err(e),
        }
    }
    let mut headers = Vec::new();
    match token {
        Some(t) => headers.push(("x-aws-ec2-metadata-token".to_string(), t)),
        None if settings.v1_disabled => {
            return Err(SessionError::new(
                ErrorKind::General,
                "Error retrieving metadata: Unable to retrieve token for use in IMDSv2 call and IMDSv1 has been disabled",
            ));
        }
        None => {}
    }
    headers.push(user_agent);
    let get = async |path: &str, json: bool| -> Result<Option<String>, SessionError> {
        let url = settings.url(path);
        for _ in 0..settings.attempts {
            match send(&http, "GET", &url, &headers).await {
                Ok(r) => {
                    let text = String::from_utf8_lossy(&r.body).into_owned();
                    if r.status == 200
                        && !r.body.is_empty()
                        && (!json || Value::parse_json(&text).is_ok())
                    {
                        return Ok(Some(text));
                    }
                }
                Err(None) => {}
                Err(Some(e)) => return Err(e),
            }
        }
        Ok(None)
    };
    let path = "latest/meta-data/iam/security-credentials/";
    let Some(role_name) = get(path, false).await? else {
        return Ok(None);
    };
    let Some(text) = get(&format!("{path}{role_name}"), true).await? else {
        return Ok(None);
    };
    let credentials = Value::parse_json(&text).unwrap_or_default();
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
    Ok(Some(RoleCredentials {
        access_key: field("AccessKeyId").unwrap_or_default(),
        secret_key: field("SecretAccessKey").unwrap_or_default(),
        token: field("Token").unwrap_or_default(),
        expiry_time: field("Expiration").unwrap_or_default(),
    }))
}

/// A minimal HTTP/1.1 exchange. `Err(true)` is a read timeout.
fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<(u16, String), bool> {
    let parsed = url::Url::parse(url).map_err(|_| false)?;
    let host = parsed.host_str().ok_or(false)?.to_string();
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addr = (host.trim_start_matches('[').trim_end_matches(']'), port)
        .to_socket_addrs()
        .map_err(|_| false)?
        .next()
        .ok_or(false)?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|_| false)?;
    stream.set_read_timeout(Some(timeout)).map_err(|_| false)?;
    stream.set_write_timeout(Some(timeout)).map_err(|_| false)?;
    let host_header = match parsed.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.clone(),
    };
    let mut req = format!(
        "{method} {} HTTP/1.1\r\nHost: {host_header}\r\nAccept-Encoding: identity\r\n",
        parsed.path()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if method == "PUT" {
        req.push_str("Content-Length: 0\r\n");
    }
    req.push_str("Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).map_err(|_| false)?;
    let mut buf = Vec::new();
    if let Err(e) = stream.read_to_end(&mut buf) {
        let timed_out = matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        );
        if buf.is_empty() {
            return Err(timed_out);
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").ok_or(false)?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or(false)?;
    Ok((status, body.to_string()))
}
