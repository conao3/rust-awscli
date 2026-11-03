//! Cross-region presigned URLs: `botocore/handlers.py`
//! `inject_presigned_url_ec2`, `inject_presigned_url_rds`,
//! `_get_cross_region_presigned_url`, signed by
//! `RequestSigner.generate_presigned_url` with `auth.SigV4QueryAuth`.

use anyhow::Result;
use chrono::{DateTime, Utc};

use super::unquote;
use crate::protocol::HttpRequest;
use crate::session::sign::{Credentials, PayloadContext, SigV4, SignableRequest};
use crate::session::url::{SplitUrl, percent_encode_sequence, urlsplit, urlunsplit};

/// What the handlers read from the request signer.
pub struct PresignEnv<'a> {
    /// `request_dict['url']`.
    pub url: &'a str,
    /// `request_signer._region_name` (the destination region).
    pub region: Option<&'a str>,
    /// The signing name of the request's signing context.
    pub signing_name: &'a str,
    /// `None` when requests are not signed (`--no-sign-request`).
    pub credentials: &'a dyn Fn() -> Result<Option<Credentials>>,
    pub unsigned: bool,
    pub now: DateTime<Utc>,
}

/// The query-protocol body as an ordered dict.
type Body = Vec<(String, String)>;

fn parse_body(body: &[u8]) -> Body {
    let text = String::from_utf8_lossy(body);
    if text.is_empty() {
        return Vec::new();
    }
    text.split('&')
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (unquote_plus(k), unquote_plus(v))
        })
        .collect()
}

fn unquote_plus(s: &str) -> String {
    unquote(&s.replace('+', " "))
}

/// `urlencode(body)` as `AWSRequest` prepares a dict body.
fn encode_body(body: &Body) -> Vec<u8> {
    body.iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                crate::protocol::xml_common::quote_plus(k.as_bytes()),
                crate::protocol::xml_common::quote_plus(v.as_bytes())
            )
        })
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes()
}

fn get<'a>(body: &'a Body, key: &str) -> Option<&'a str> {
    body.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// `body[key] = value` on a dict: replaced in place, or appended.
fn set(body: &mut Body, key: &str, value: String) {
    match body.iter_mut().find(|(k, _)| k == key) {
        Some(entry) => entry.1 = value,
        None => body.push((key.to_string(), value)),
    }
}

/// `inject_presigned_url_ec2`.
pub(super) fn inject_presigned_url_ec2(
    request: &mut HttpRequest,
    env: &PresignEnv<'_>,
) -> Result<()> {
    let mut body = parse_body(&request.body);
    if get(&body, "PresignedUrl").is_some() {
        return Ok(());
    }
    let src = get(&body, "SourceRegion").map(str::to_string);
    let dest = env.region.map(str::to_string);
    let url = cross_region_presigned_url(&body, env, src.as_deref(), dest.as_deref())?;
    set(&mut body, "PresignedUrl", url);
    set(&mut body, "DestinationRegion", dest.unwrap_or_default());
    request.body = encode_body(&body);
    Ok(())
}

/// `inject_presigned_url_rds`.
pub(super) fn inject_presigned_url_rds(
    request: &mut HttpRequest,
    env: &PresignEnv<'_>,
) -> Result<()> {
    let mut body = parse_body(&request.body);
    let Some(src) = get(&body, "SourceRegion").map(str::to_string) else {
        return Ok(());
    };
    let dest = env.region.map(str::to_string);
    body.retain(|(k, _)| k != "SourceRegion");
    if get(&body, "PreSignedUrl").is_none() {
        let url = cross_region_presigned_url(&body, env, Some(&src), dest.as_deref())?;
        set(&mut body, "PreSignedUrl", url);
    }
    request.body = encode_body(&body);
    Ok(())
}

/// `_get_cross_region_presigned_url`.
fn cross_region_presigned_url(
    body: &Body,
    env: &PresignEnv<'_>,
    source_region: Option<&str>,
    destination_region: Option<&str>,
) -> Result<String> {
    let mut body = body.clone();
    set(
        &mut body,
        "DestinationRegion",
        destination_region.unwrap_or_default().to_string(),
    );
    let url = match (destination_region, source_region) {
        (Some(dest), Some(src)) => env.url.replace(dest, src),
        _ => env.url.to_string(),
    };
    if env.unsigned {
        return Ok(url);
    }
    let credentials =
        (env.credentials)()?.ok_or_else(crate::session::SessionError::no_credentials)?;
    Ok(presign_url(
        &url,
        &body,
        &credentials,
        env.signing_name,
        source_region.unwrap_or_default(),
        env.now,
    ))
}

/// `SigV4QueryAuth.add_auth` on a `GET` of `url` whose dict body is
/// `body`, then `request.url`.
pub(super) fn presign_url(
    url: &str,
    body: &Body,
    credentials: &Credentials,
    signing_name: &str,
    region: &str,
    now: DateTime<Utc>,
) -> String {
    let signer = SigV4 {
        credentials: credentials.clone(),
        service_name: signing_name.to_string(),
        region_name: region.to_string(),
        s3: false,
    };
    let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let scope = format!(
        "{}/{}/{region}/{signing_name}/aws4_request",
        credentials.access_key,
        &timestamp[..8]
    );
    let mut auth_params = vec![
        (
            "X-Amz-Algorithm".to_string(),
            "AWS4-HMAC-SHA256".to_string(),
        ),
        ("X-Amz-Credential".to_string(), scope),
        ("X-Amz-Date".to_string(), timestamp.clone()),
        ("X-Amz-Expires".to_string(), "3600".to_string()),
        ("X-Amz-SignedHeaders".to_string(), "host".to_string()),
    ];
    if let Some(token) = &credentials.token {
        auth_params.push(("X-Amz-Security-Token".to_string(), token.clone()));
    }
    let parts = urlsplit(url);
    let mut query: Body = Vec::new();
    for pair in parts.query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = unquote_plus(k);
        if get(&query, &k).is_none() {
            query.push((k, unquote_plus(v)));
        }
    }
    for (k, v) in body {
        set(&mut query, k, v.clone());
    }
    let mut new_query = String::new();
    if !query.is_empty() {
        new_query = percent_encode_sequence(&query) + "&";
    }
    new_query.push_str(&percent_encode_sequence(&auth_params));
    let presigned = urlunsplit(&SplitUrl {
        query: new_query,
        ..parts
    });
    let request = SignableRequest {
        method: "GET".to_string(),
        url: presigned.clone(),
        headers: Vec::new(),
        body: Vec::new(),
    };
    let canonical = signer.canonical_request(&request, &PayloadContext::default());
    let string_to_sign = signer.string_to_sign(&timestamp, &canonical);
    let signature = signer.signature(&timestamp, &string_to_sign);
    format!("{presigned}&X-Amz-Signature={signature}")
}
