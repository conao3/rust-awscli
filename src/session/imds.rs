//! The region from the EC2 instance metadata service, the last provider
//! of the CLI's region chain (`awscli/utils.py` `IMDSRegionProvider`,
//! `botocore/utils.py` `IMDSFetcher`, `InstanceMetadataRegionFetcher`).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::config::Config;

const METADATA_BASE_URL: &str = "http://169.254.169.254/";
const METADATA_BASE_URL_IPV6: &str = "http://[fd00:ec2::254]/";

/// `InstanceMetadataRegionFetcher.retrieve_region`: `None` when IMDS is
/// disabled (`AWS_EC2_METADATA_DISABLED=true`), unreachable, or gives no
/// usable answer.
pub fn fetch_region(config: &Config) -> Option<String> {
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
    let v1_disabled = config.get_bool("ec2_metadata_v1_disabled").ok().flatten() == Some(true);
    let timeout = Duration::from_secs(timeout);
    let url = |path: &str| {
        let sep = if base.ends_with('/') { "" } else { "/" };
        format!("{base}{sep}{path}")
    };

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
