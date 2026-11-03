//! Request signing: botocore's signer selection (`botocore/client.py`
//! `_resolve_signature_version`, `botocore/auth.py` `resolve_auth_type`,
//! `resolve_auth_scheme_preference`, `botocore/regions.py`
//! `auth_schemes_to_signing_ctx`, `botocore/handlers.py`
//! `set_operation_specific_signer`, `disable_signing`,
//! `botocore/signers.py` `RequestSigner.sign`), SigV4 (`botocore/auth.py`
//! `SigV4Auth`, `S3SigV4Auth`), SigV4a (`botocore/crt/auth.py`
//! `CrtSigV4AsymAuth`, `CrtS3SigV4AsymAuth`, with `aws-sigv4`), and bearer
//! tokens (`BearerAuth`).

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use super::error::{ErrorKind, SessionError};
use super::url::{UrlParts, quote, urlsplit};

pub const EMPTY_SHA256_HASH: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
pub const STREAMING_UNSIGNED_PAYLOAD_TRAILER: &str = "STREAMING-UNSIGNED-PAYLOAD-TRAILER";
const SIGV4_TIMESTAMP: &str = "%Y%m%dT%H%M%SZ";

/// `SIGNED_HEADERS_BLACKLIST`.
pub const SIGNED_HEADERS_BLACKLIST: &[&str] = &[
    "connection",
    "expect",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "user-agent",
    "x-amzn-trace-id",
];

/// `S3_SIGNING_NAMES` (`botocore/handlers.py`).
pub const S3_SIGNING_NAMES: &[&str] = &["s3", "s3-outposts", "s3-object-lambda", "s3express"];

/// Header list operations with `HTTPHeaders` (`email.message.Message`)
/// semantics: case-insensitive lookups, `del` removes every occurrence,
/// assignment appends.
pub mod headers {
    pub fn get<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
        h.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn contains(h: &[(String, String)], name: &str) -> bool {
        h.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
    }

    pub fn remove(h: &mut Vec<(String, String)>, name: &str) {
        h.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    }

    /// `headers[name] = value` on a `dict`-like header map (botocore's
    /// request dict headers): replace in place, or append.
    pub fn set(h: &mut Vec<(String, String)>, name: &str, value: &str) {
        if let Some(entry) = h.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            entry.1 = value.to_string();
            h.retain_mut({
                let mut seen = false;
                move |(k, _)| {
                    if k.eq_ignore_ascii_case(name) {
                        let keep = !seen;
                        seen = true;
                        keep
                    } else {
                        true
                    }
                }
            });
        } else {
            h.push((name.to_string(), value.to_string()));
        }
    }

    /// `del headers[name]; headers[name] = value` on `HTTPHeaders`.
    pub fn replace(h: &mut Vec<(String, String)>, name: &str, value: &str) {
        remove(h, name);
        h.push((name.to_string(), value.to_string()));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub token: Option<String>,
}

/// The `request.context` entries the signers read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadContext {
    /// `context['payload_signing_enabled']` (default true).
    pub payload_signing_enabled: bool,
    /// `context['has_streaming_input']`.
    pub has_streaming_input: bool,
    /// `client_config.s3['payload_signing_enabled']`.
    pub s3_payload_signing_enabled: Option<bool>,
    /// The checksum is sent in a trailer
    /// (`context['checksum']['request_algorithm']['in'] == 'trailer'`).
    pub checksum_in_trailer: bool,
    /// The header the request checksum is sent in, when it is sent in a
    /// header (`request_algorithm['name']`).
    pub checksum_header: Option<String>,
}

impl Default for PayloadContext {
    fn default() -> Self {
        PayloadContext {
            payload_signing_enabled: true,
            has_streaming_input: false,
            s3_payload_signing_enabled: None,
            checksum_in_trailer: false,
            checksum_header: None,
        }
    }
}

/// A request being signed: botocore's `AWSRequest` before `prepare()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignableRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], msg: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(msg.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// `utils.remove_dot_segments` (via `normalize_url_path`).
pub fn normalize_url_path(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    let mut out: Vec<&str> = Vec::new();
    for x in path.split('/') {
        if !x.is_empty() && x != "." {
            if x == ".." {
                out.pop();
            } else {
                out.push(x);
            }
        }
    }
    let first = if path.starts_with('/') { "/" } else { "" };
    let last = if path.ends_with('/') && !out.is_empty() {
        "/"
    } else {
        ""
    };
    format!("{first}{}{last}", out.join("/"))
}

/// `SigV4Auth` (`s3 == false`) and `S3SigV4Auth` (`s3 == true`).
#[derive(Debug, Clone)]
pub struct SigV4 {
    pub credentials: Credentials,
    pub service_name: String,
    pub region_name: String,
    pub s3: bool,
}

impl SigV4 {
    /// `headers_to_sign`: lowercased name/value pairs, duplicates kept,
    /// plus `host` from the URL.
    fn headers_to_sign(&self, req: &SignableRequest) -> Vec<(String, String)> {
        let mut map: Vec<(String, String)> = req
            .headers
            .iter()
            .filter_map(|(k, v)| {
                let l = k.to_lowercase();
                (!SIGNED_HEADERS_BLACKLIST.contains(&l.as_str())).then(|| (l, v.clone()))
            })
            .collect();
        if !map.iter().any(|(k, _)| k == "host") {
            let host = UrlParts::parse(&req.url)
                .map(|p| p.host_header())
                .unwrap_or_default();
            map.push(("host".to_string(), host));
        }
        map
    }

    fn sorted_names(headers: &[(String, String)]) -> Vec<String> {
        let mut names: Vec<String> = headers.iter().map(|(k, _)| k.clone()).collect();
        names.sort();
        names.dedup();
        names
    }

    fn canonical_headers(headers: &[(String, String)]) -> String {
        Self::sorted_names(headers)
            .iter()
            .map(|name| {
                let value = headers
                    .iter()
                    .filter(|(k, _)| k == name)
                    .map(|(_, v)| v.split_whitespace().collect::<Vec<_>>().join(" "))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{name}:{value}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn signed_headers(headers: &[(String, String)]) -> String {
        let mut names: Vec<String> = Self::sorted_names(headers)
            .into_iter()
            .map(|n| n.trim().to_string())
            .collect();
        names.sort();
        names.join(";")
    }

    /// `_canonical_query_string_url`.
    fn canonical_query_string(url: &str) -> String {
        let query = urlsplit(url).query;
        if query.is_empty() {
            return String::new();
        }
        let mut pairs: Vec<(&str, &str)> = query
            .split('&')
            .map(|p| p.split_once('=').unwrap_or((p, "")))
            .collect();
        pairs.sort();
        pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    }

    fn should_sha256_sign_payload(&self, req: &SignableRequest, ctx: &PayloadContext) -> bool {
        if self.s3 {
            if let Some(sign) = ctx.s3_payload_signing_enabled {
                return sign;
            }
            let checksum_header = ctx.checksum_header.as_deref().unwrap_or("Content-MD5");
            if !req.url.starts_with("https") || !headers::contains(&req.headers, checksum_header) {
                return true;
            }
            if ctx.has_streaming_input {
                return false;
            }
        }
        if !req.url.starts_with("https") {
            return true;
        }
        ctx.payload_signing_enabled
    }

    /// `payload`.
    fn payload(&self, req: &SignableRequest, ctx: &PayloadContext) -> String {
        if ctx.checksum_in_trailer {
            STREAMING_UNSIGNED_PAYLOAD_TRAILER.to_string()
        } else if !self.should_sha256_sign_payload(req, ctx) {
            UNSIGNED_PAYLOAD.to_string()
        } else if !req.body.is_empty() {
            sha256_hex(&req.body)
        } else {
            EMPTY_SHA256_HASH.to_string()
        }
    }

    /// `canonical_request`.
    pub fn canonical_request(&self, req: &SignableRequest, ctx: &PayloadContext) -> String {
        let raw_path = urlsplit(&req.url).path;
        let path = if self.s3 {
            raw_path
        } else {
            quote(&normalize_url_path(&raw_path), "/~")
        };
        let headers_to_sign = self.headers_to_sign(req);
        let body_checksum = match headers::get(&req.headers, "X-Amz-Content-SHA256") {
            Some(v) => v.to_string(),
            None => self.payload(req, ctx),
        };
        [
            req.method.to_uppercase(),
            path,
            Self::canonical_query_string(&req.url),
            format!("{}\n", Self::canonical_headers(&headers_to_sign)),
            Self::signed_headers(&headers_to_sign),
            body_checksum,
        ]
        .join("\n")
    }

    fn credential_scope(&self, timestamp: &str) -> String {
        format!(
            "{}/{}/{}/aws4_request",
            &timestamp[..8],
            self.region_name,
            self.service_name
        )
    }

    pub fn string_to_sign(&self, timestamp: &str, canonical_request: &str) -> String {
        [
            "AWS4-HMAC-SHA256".to_string(),
            timestamp.to_string(),
            self.credential_scope(timestamp),
            sha256_hex(canonical_request.as_bytes()),
        ]
        .join("\n")
    }

    pub fn signature(&self, timestamp: &str, string_to_sign: &str) -> String {
        let k_date = hmac(
            format!("AWS4{}", self.credentials.secret_key).as_bytes(),
            &timestamp[..8],
        );
        let k_region = hmac(&k_date, &self.region_name);
        let k_service = hmac(&k_region, &self.service_name);
        let k_signing = hmac(&k_service, "aws4_request");
        hex::encode(hmac(&k_signing, string_to_sign))
    }

    /// `_modify_request_before_signing` (and the S3 override).
    fn modify_request_before_signing(
        &self,
        req: &mut SignableRequest,
        ctx: &PayloadContext,
        timestamp: &str,
        now: DateTime<Utc>,
    ) {
        headers::remove(&mut req.headers, "Authorization");
        if headers::contains(&req.headers, "Date") {
            headers::replace(&mut req.headers, "Date", &formatdate(now));
            headers::remove(&mut req.headers, "X-Amz-Date");
        } else {
            headers::replace(&mut req.headers, "X-Amz-Date", timestamp);
        }
        if let Some(token) = self.credentials.token.as_deref().filter(|t| !t.is_empty()) {
            headers::replace(&mut req.headers, "X-Amz-Security-Token", token);
        }
        if !ctx.payload_signing_enabled {
            headers::replace(&mut req.headers, "X-Amz-Content-SHA256", UNSIGNED_PAYLOAD);
        }
        if self.s3 {
            headers::remove(&mut req.headers, "X-Amz-Content-SHA256");
            let payload = self.payload(req, ctx);
            req.headers
                .push(("X-Amz-Content-SHA256".to_string(), payload));
        }
    }

    /// `add_auth` at the instant `now`.
    pub fn add_auth(&self, req: &mut SignableRequest, ctx: &PayloadContext, now: DateTime<Utc>) {
        let timestamp = now.format(SIGV4_TIMESTAMP).to_string();
        self.modify_request_before_signing(req, ctx, &timestamp, now);
        let canonical_request = self.canonical_request(req, ctx);
        let string_to_sign = self.string_to_sign(&timestamp, &canonical_request);
        let signature = self.signature(&timestamp, &string_to_sign);
        let headers_to_sign = self.headers_to_sign(req);
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={signature}",
            self.credentials.access_key,
            self.credential_scope(&timestamp),
            Self::signed_headers(&headers_to_sign),
        );
        req.headers.push(("Authorization".to_string(), auth));
    }
}

/// `email.utils.formatdate(timeval)` (UTC, `-0000`).
fn formatdate(now: DateTime<Utc>) -> String {
    now.format("%a, %d %b %Y %H:%M:%S -0000").to_string()
}

/// `CrtSigV4AsymAuth` (`s3 == false`) and `CrtS3SigV4AsymAuth`
/// (`s3 == true`), signed with `aws-sigv4`'s SigV4a.
#[derive(Debug, Clone)]
pub struct SigV4a {
    pub credentials: Credentials,
    pub service_name: String,
    /// The region set (`*` or a comma-separated list).
    pub region_set: String,
    pub s3: bool,
}

impl SigV4a {
    pub fn add_auth(
        &self,
        req: &mut SignableRequest,
        ctx: &PayloadContext,
        now: DateTime<Utc>,
    ) -> Result<(), SessionError> {
        use aws_sigv4::http_request::{
            PayloadChecksumKind, PercentEncodingMode, SignableBody, SignableRequest as AwsReq,
            SigningSettings, UriPathNormalizationMode, sign,
        };
        use aws_smithy_runtime_api::client::identity::Identity;

        let existing_sha256 = if self.s3 {
            None
        } else {
            headers::get(&req.headers, "X-Amz-Content-SHA256").map(str::to_string)
        };
        for h in [
            "Authorization",
            "X-Amz-Date",
            "X-Amz-Content-SHA256",
            "X-Amz-Security-Token",
            "X-Amz-Region-Set",
        ] {
            headers::remove(&mut req.headers, h);
        }
        if !headers::contains(&req.headers, "host") {
            let host = UrlParts::parse(&req.url)
                .map(|p| p.host_header())
                .unwrap_or_default();
            req.headers.push(("host".to_string(), host));
        }

        let v4 = SigV4 {
            credentials: self.credentials.clone(),
            service_name: self.service_name.clone(),
            region_name: String::new(),
            s3: self.s3,
        };
        let explicit_payload = if ctx.checksum_in_trailer {
            Some(STREAMING_UNSIGNED_PAYLOAD_TRAILER.to_string())
        } else if v4.should_sha256_sign_payload(req, ctx) {
            existing_sha256
        } else {
            Some(UNSIGNED_PAYLOAD.to_string())
        };
        let add_sha256_header = self.s3 || explicit_payload.is_some();

        let mut settings = SigningSettings::default();
        settings.excluded_headers = Some(
            SIGNED_HEADERS_BLACKLIST
                .iter()
                .map(|h| std::borrow::Cow::Borrowed(*h))
                .chain([std::borrow::Cow::Borrowed("authorization")])
                .collect(),
        );
        if self.s3 {
            settings.percent_encoding_mode = PercentEncodingMode::Single;
            settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
        }
        if add_sha256_header {
            settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        }

        let creds = aws_credential_types::Credentials::new(
            &self.credentials.access_key,
            &self.credentials.secret_key,
            self.credentials.token.clone(),
            None,
            "awsr",
        );
        let identity: Identity = creds.into();
        let params = aws_sigv4::sign::v4a::SigningParams::builder()
            .identity(&identity)
            .region_set(&self.region_set)
            .name(&self.service_name)
            .time(now.into())
            .settings(settings)
            .build()
            .map_err(|e| SessionError::new(ErrorKind::General, e.to_string()))?
            .into();
        let body = match &explicit_payload {
            Some(p) => SignableBody::Precomputed(p.clone()),
            None => SignableBody::Bytes(&req.body),
        };
        let signable = AwsReq::new(
            &req.method,
            &req.url,
            req.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            body,
        )
        .map_err(|e| SessionError::new(ErrorKind::General, e.to_string()))?;
        let (instructions, _) = sign(signable, &params)
            .map_err(|e| SessionError::new(ErrorKind::General, e.to_string()))?
            .into_parts();
        for (name, value) in instructions.headers() {
            let name = match name {
                "x-amz-date" => "X-Amz-Date",
                "x-amz-region-set" => "X-Amz-Region-Set",
                "x-amz-security-token" => "X-Amz-Security-Token",
                "x-amz-content-sha256" => "x-amz-content-sha256",
                "authorization" => "Authorization",
                other => other,
            };
            headers::replace(&mut req.headers, name, value);
        }
        Ok(())
    }
}

/// The signer botocore picks for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureVersion {
    /// `botocore.UNSIGNED`.
    Unsigned,
    /// `v4`, `s3v4`.
    V4 { s3: bool },
    /// `v4a`, `s3v4a`.
    V4a { s3: bool },
    /// `bearer`.
    Bearer,
    /// Any other name from `AUTH_TYPE_MAPS` (`v2`, `v3`,
    /// `v4-s3express`, ...), signed as SigV4 here.
    Other(String),
}

impl SignatureVersion {
    pub fn from_name(name: &str) -> SignatureVersion {
        match name {
            "v4" => SignatureVersion::V4 { s3: false },
            "s3v4" => SignatureVersion::V4 { s3: true },
            "v4a" => SignatureVersion::V4a { s3: false },
            "s3v4a" => SignatureVersion::V4a { s3: true },
            "bearer" => SignatureVersion::Bearer,
            "none" => SignatureVersion::Unsigned,
            other => SignatureVersion::Other(other.to_string()),
        }
    }
}

/// The names in `AUTH_TYPE_MAPS`.
const AUTH_TYPE_MAPS: &[&str] = &[
    "v2",
    "v4",
    "v4a",
    "v4-query",
    "v3",
    "v3https",
    "s3v4",
    "s3v4-query",
    "s3v4-presign-post",
    "s3v4a",
    "s3v4a-query",
    "v4-s3express",
    "v4-s3express-query",
    "v4-s3express-presign-post",
    "bearer",
];

/// `AUTH_TYPE_TO_SIGNATURE_VERSION`.
fn auth_trait_to_signature_version(t: &str) -> Option<&'static str> {
    match t {
        "aws.auth#sigv4" => Some("v4"),
        "aws.auth#sigv4a" => Some("v4a"),
        "smithy.api#httpBearerAuth" => Some("bearer"),
        "smithy.api#noAuth" => Some("none"),
        _ => None,
    }
}

/// `auth.resolve_auth_type`.
pub fn resolve_auth_type(auth: &[String]) -> Result<String, SessionError> {
    for t in auth {
        match auth_trait_to_signature_version(t) {
            Some("none") => return Ok("none".to_string()),
            Some(v) if AUTH_TYPE_MAPS.contains(&v) => return Ok(v.to_string()),
            Some(_) => {}
            None => {
                return Err(SessionError::new(
                    ErrorKind::Signature,
                    format!("Unknown Signature Version: {t}."),
                ));
            }
        }
    }
    Err(SessionError::new(
        ErrorKind::Signature,
        format!(
            "Signature version(s) are not supported: {}",
            py_list_repr(auth)
        ),
    ))
}

/// `auth.resolve_auth_scheme_preference`.
pub fn resolve_auth_scheme_preference(
    preference: &[String],
    auth_options: &[String],
) -> Result<String, SessionError> {
    let supported: Vec<&str> = auth_options
        .iter()
        .map(|s| s.rsplit('#').next().unwrap_or(s))
        .collect();
    let mut seen = Vec::new();
    for s in preference
        .iter()
        .map(String::as_str)
        .chain(supported.iter().copied())
    {
        if supported.contains(&s) && !seen.contains(&s) {
            seen.push(s);
        }
    }
    for scheme in seen {
        let sig = match scheme {
            "sigv4" => "v4",
            "sigv4a" => "v4a",
            "httpBearerAuth" => "bearer",
            "noAuth" => return Ok("none".to_string()),
            _ => continue,
        };
        if AUTH_TYPE_MAPS.contains(&sig) {
            return Ok(sig.to_string());
        }
    }
    let mut sorted: Vec<&str> = supported.clone();
    sorted.sort();
    Err(SessionError::new(
        ErrorKind::Signature,
        format!(
            "Signature version(s) are not supported: {}",
            sorted.join(", ")
        ),
    ))
}

fn py_list_repr(items: &[String]) -> String {
    format!(
        "[{}]",
        items
            .iter()
            .map(|s| super::error::py_repr_str(s))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The inputs of signer selection.
#[derive(Debug, Clone, Default)]
pub struct AuthInputs<'a> {
    /// The botocore service name.
    pub service_name: &'a str,
    pub endpoint_prefix: &'a str,
    /// `service_model.signing_name`.
    pub signing_name: &'a str,
    pub service_id: &'a str,
    pub operation_name: &'a str,
    /// Service `metadata.auth`.
    pub service_auth: Option<Vec<String>>,
    /// Service `metadata.signatureVersion`.
    pub service_signature_version: Option<String>,
    /// Operation `auth`.
    pub operation_auth: Option<Vec<String>>,
    /// Operation `authtype`.
    pub operation_authtype: Option<String>,
    /// Operation `unsignedPayload`.
    pub unsigned_payload: bool,
    /// The endpoint's `properties.authSchemes`.
    pub auth_schemes: Option<&'a serde_json::Value>,
    /// `--no-sign-request`.
    pub no_sign_request: bool,
    /// `[<service>] signature_version` from the scoped config.
    pub configured_signature_version: Option<String>,
    /// `auth_scheme_preference`.
    pub auth_scheme_preference: Option<String>,
    /// `sigv4a_signing_region_set`.
    pub sigv4a_signing_region_set: Option<String>,
    /// The client region.
    pub region: Option<String>,
}

/// The resolved signer and signing context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerChoice {
    pub version: SignatureVersion,
    pub signing_name: String,
    pub region: Option<String>,
    /// `context['payload_signing_enabled']`.
    pub payload_signing_enabled: bool,
}

/// `ClientCreator._resolve_signature_version` (`ClientEndpointBridge`):
/// the client's default signature version.
fn client_signature_version(inputs: &AuthInputs<'_>) -> Result<Option<String>, SessionError> {
    if inputs.no_sign_request {
        return Ok(None);
    }
    if let Some(v) = &inputs.configured_signature_version {
        return Ok(Some(v.clone()));
    }
    if matches!(inputs.endpoint_prefix, "s3" | "s3-control") {
        return Ok(Some("s3v4".to_string()));
    }
    let service_version = match &inputs.service_auth {
        Some(auth) if !auth.is_empty() => Some(resolve_auth_type(auth)?),
        _ => inputs.service_signature_version.clone(),
    };
    match service_version {
        Some(v) if AUTH_TYPE_MAPS.contains(&v.as_str()) => Ok(Some(v)),
        Some(v) if v == "none" => Ok(None),
        Some(v) => Err(SessionError::new(
            ErrorKind::Signature,
            format!("Unknown Signature Version: {}.", py_list_repr(&[v])),
        )),
        None => Ok(Some("v4".to_string())),
    }
}

/// Choose the signer for one request, as `_make_api_call`,
/// `_resolve_endpoint_ruleset`, the `choose-signer` handlers, and
/// `RequestSigner.sign` do.
pub fn choose_signer(inputs: &AuthInputs<'_>) -> Result<SignerChoice, SessionError> {
    let client_version = client_signature_version(inputs)?;
    // `_make_api_call`: the operation's auth type.
    let mut auth_type: Option<String> = match &inputs.operation_auth {
        Some(auth) if !auth.is_empty() => match &inputs.auth_scheme_preference {
            Some(pref) if !pref.is_empty() => {
                let prefs: Vec<String> = pref.split(',').map(str::to_string).collect();
                Some(resolve_auth_scheme_preference(&prefs, auth)?)
            }
            _ => Some(resolve_auth_type(auth)?),
        },
        _ => inputs.operation_authtype.clone(),
    };
    let mut signing_name: Option<String> = None;
    let mut signing_region: Option<String> = None;

    // `auth_schemes_to_signing_ctx`.
    if let Some(schemes) = inputs.auth_schemes.and_then(|s| s.as_array())
        && !schemes.is_empty()
    {
        if inputs.no_sign_request {
            auth_type = Some("none".to_string());
        } else {
            let strip = |n: &str| n.strip_prefix("sig").unwrap_or(n).to_string();
            let named: Vec<(String, &serde_json::Value)> = schemes
                .iter()
                .map(|s| (strip(s["name"].as_str().unwrap_or_default()), s))
                .collect();
            let chosen = match inputs
                .auth_scheme_preference
                .as_deref()
                .filter(|p| !p.is_empty())
            {
                Some(pref) => {
                    let prefs: Vec<String> = pref.split(',').map(str::to_string).collect();
                    let available: Vec<String> = schemes
                        .iter()
                        .map(|s| {
                            let n = s["name"].as_str().unwrap_or_default();
                            n.rsplit('#').next().unwrap_or(n).to_string()
                        })
                        .collect();
                    let name = resolve_auth_scheme_preference(&prefs, &available)?;
                    let scheme = named
                        .iter()
                        .find(|(n, _)| strip(n.rsplit('#').next().unwrap_or(n)) == name)
                        .map(|(_, s)| *s)
                        .ok_or_else(|| SessionError::new(ErrorKind::Signature, name.clone()))?;
                    (name, scheme)
                }
                None => named
                    .iter()
                    .find(|(n, _)| AUTH_TYPE_MAPS.contains(&n.as_str()))
                    .map(|(n, s)| (n.clone(), *s))
                    .ok_or_else(|| {
                        SessionError::new(
                            ErrorKind::Signature,
                            format!(
                                "Unknown Signature Version: {}.",
                                named
                                    .iter()
                                    .map(|(n, _)| n.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        )
                    })?,
            };
            let (name, scheme) = chosen;
            if let Some(r) = scheme.get("signingRegion").and_then(|v| v.as_str()) {
                signing_region = Some(r.to_string());
            } else if let Some(set) = scheme.get("signingRegionSet").and_then(|v| v.as_array())
                && !set.is_empty()
            {
                signing_region = Some(
                    set.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
            if let Some(n) = scheme.get("signingName").and_then(|v| v.as_str()) {
                signing_name = Some(n.to_string());
            }
            auth_type = Some(name);
        }
    }

    let signing_name = signing_name.unwrap_or_else(|| inputs.signing_name.to_string());
    let mut payload_signing_enabled = true;

    // `_choose_signer` with the `choose-signer` handlers.
    let disabled = matches!(
        (inputs.service_id, inputs.operation_name),
        (
            "cognito-identity",
            "GetId" | "GetOpenIdToken" | "UnlinkIdentity" | "GetCredentialsForIdentity"
        ) | ("sts", "AssumeRoleWithSAML" | "AssumeRoleWithWebIdentity")
    );
    let mut sigv4a_region: Option<String> = None;
    let handler_response: Option<String> = if disabled {
        Some("none".to_string())
    } else if let Some(t) = auth_type.as_deref().filter(|t| !t.is_empty()) {
        // `set_operation_specific_signer`.
        if t == "none" {
            Some("none".to_string())
        } else if t == "bearer" {
            Some("bearer".to_string())
        } else {
            if inputs.unsigned_payload || t == "v4-unsigned-body" {
                payload_signing_enabled = false;
            }
            if t.starts_with("v4") {
                if t == "v4-s3express" {
                    Some(t.to_string())
                } else {
                    let v = if t == "v4a" {
                        sigv4a_region = Some(sigv4a_region_set(inputs, signing_region.as_deref()));
                        "v4a"
                    } else {
                        "v4"
                    };
                    Some(if S3_SIGNING_NAMES.contains(&signing_name.as_str()) {
                        format!("s3{v}")
                    } else {
                        v.to_string()
                    })
                }
            } else {
                None
            }
        }
    } else {
        None
    };
    let version_name = match handler_response {
        Some(v) => Some(v),
        None => auth_type.clone().or(client_version),
    };
    let version = match version_name.as_deref() {
        None | Some("none") => SignatureVersion::Unsigned,
        Some(v) => SignatureVersion::from_name(v),
    };
    let version = if inputs.no_sign_request {
        SignatureVersion::Unsigned
    } else {
        version
    };
    let region = match &version {
        SignatureVersion::V4a { .. } => Some(
            sigv4a_region.unwrap_or_else(|| sigv4a_region_set(inputs, signing_region.as_deref())),
        ),
        _ => signing_region.or_else(|| inputs.region.clone()),
    };
    Ok(SignerChoice {
        version,
        signing_name,
        region,
        payload_signing_enabled,
    })
}

/// `_resolve_sigv4a_region`.
fn sigv4a_region_set(inputs: &AuthInputs<'_>, signing_region: Option<&str>) -> String {
    inputs
        .sigv4a_signing_region_set
        .clone()
        .filter(|r| !r.is_empty())
        .or_else(|| signing_region.map(str::to_string).filter(|r| !r.is_empty()))
        .unwrap_or_else(|| "*".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn creds(token: Option<&str>) -> Credentials {
        Credentials {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            token: token.map(str::to_string),
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 3, 12, 0, 0).unwrap()
    }

    fn pairs(v: &serde_json::Value) -> Vec<(String, String)> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p[0].as_str().unwrap().to_string(),
                    p[1].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// Goldens from `botocore.auth.SigV4Auth` / `S3SigV4Auth.add_auth`
    /// with `datetime.utcnow` fixed to 2026-11-03T12:00:00
    /// (`testdata/sigv4_golden.py`).
    #[test]
    fn sigv4_matches_botocore() {
        for line in include_str!("testdata/sigv4.jsonl").lines() {
            let case: serde_json::Value = serde_json::from_str(line).unwrap();
            let signer = SigV4 {
                credentials: creds(case["token"].as_str()),
                service_name: case["service"].as_str().unwrap().into(),
                region_name: case["region"].as_str().unwrap().into(),
                s3: case["cls"] == "s3v4",
            };
            let ctx = PayloadContext {
                payload_signing_enabled: case["ctx"]["payload_signing_enabled"]
                    .as_bool()
                    .unwrap_or(true),
                has_streaming_input: case["ctx"]["has_streaming_input"]
                    .as_bool()
                    .unwrap_or(false),
                ..Default::default()
            };
            let mut req = SignableRequest {
                method: case["method"].as_str().unwrap().into(),
                url: case["url"].as_str().unwrap().into(),
                headers: pairs(&case["headers"]),
                body: case["body"].as_str().unwrap().as_bytes().to_vec(),
            };
            signer.add_auth(&mut req, &ctx, now());
            assert_eq!(req.headers, pairs(&case["signed"]), "{}", case["name"]);
        }
    }

    #[test]
    fn sigv4a_signs_with_region_set() {
        let signer = SigV4a {
            credentials: creds(Some("tok")),
            service_name: "cloudfront-keyvaluestore".into(),
            region_set: "*".into(),
            s3: false,
        };
        let mut req = SignableRequest {
            method: "GET".into(),
            url: "https://example.com/key-value-stores/x".into(),
            headers: vec![("User-Agent".into(), "ua".into())],
            body: Vec::new(),
        };
        signer
            .add_auth(&mut req, &PayloadContext::default(), now())
            .unwrap();
        assert_eq!(headers::get(&req.headers, "host"), Some("example.com"));
        assert_eq!(headers::get(&req.headers, "X-Amz-Region-Set"), Some("*"));
        assert_eq!(
            headers::get(&req.headers, "X-Amz-Security-Token"),
            Some("tok")
        );
        let auth = headers::get(&req.headers, "Authorization").unwrap();
        assert!(auth.starts_with(
            "AWS4-ECDSA-P256-SHA256 Credential=AKIDEXAMPLE/20261103/cloudfront-keyvaluestore/aws4_request"
        ));
    }

    fn inputs<'a>() -> AuthInputs<'a> {
        AuthInputs {
            service_name: "sts",
            endpoint_prefix: "sts",
            signing_name: "sts",
            service_id: "sts",
            operation_name: "GetCallerIdentity",
            service_signature_version: Some("v4".into()),
            region: Some("us-west-2".into()),
            ..Default::default()
        }
    }

    #[test]
    fn signer_selection() {
        let c = choose_signer(&inputs()).unwrap();
        assert_eq!(c.version, SignatureVersion::V4 { s3: false });
        assert_eq!(c.region.as_deref(), Some("us-west-2"));

        let c = choose_signer(&AuthInputs {
            operation_name: "AssumeRoleWithWebIdentity",
            ..inputs()
        })
        .unwrap();
        assert_eq!(c.version, SignatureVersion::Unsigned);

        let c = choose_signer(&AuthInputs {
            operation_auth: Some(vec!["smithy.api#noAuth".into()]),
            ..inputs()
        })
        .unwrap();
        assert_eq!(c.version, SignatureVersion::Unsigned);

        let schemes = serde_json::json!([
            {"name": "sigv4", "signingName": "s3", "signingRegion": "eu-west-1", "disableDoubleEncoding": true}
        ]);
        let s3 = AuthInputs {
            service_name: "s3",
            endpoint_prefix: "s3",
            signing_name: "s3",
            service_id: "s3",
            operation_name: "ListObjectsV2",
            ..inputs()
        };
        let c = choose_signer(&AuthInputs {
            auth_schemes: Some(&schemes),
            ..s3.clone()
        })
        .unwrap();
        assert_eq!(c.version, SignatureVersion::V4 { s3: true });
        assert_eq!(c.region.as_deref(), Some("eu-west-1"));

        let schemes = serde_json::json!([
            {"name": "sigv4a", "signingName": "s3", "signingRegionSet": ["*"]},
            {"name": "sigv4", "signingName": "s3", "signingRegion": "us-east-1"}
        ]);
        let c = choose_signer(&AuthInputs {
            auth_schemes: Some(&schemes),
            ..s3
        })
        .unwrap();
        assert_eq!(c.version, SignatureVersion::V4a { s3: true });
        assert_eq!(c.region.as_deref(), Some("*"));

        let c = choose_signer(&AuthInputs {
            no_sign_request: true,
            ..inputs()
        })
        .unwrap();
        assert_eq!(c.version, SignatureVersion::Unsigned);

        let c = choose_signer(&AuthInputs {
            operation_auth: Some(vec!["aws.auth#sigv4".into()]),
            unsigned_payload: true,
            ..inputs()
        })
        .unwrap();
        assert!(!c.payload_signing_enabled);
    }
}
