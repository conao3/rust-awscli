//! The session: configuration, credentials, signing, and sending requests
//! with retries (`botocore/session.py`, `botocore/client.py`
//! `_make_api_call`, `botocore/endpoint.py` `Endpoint._send_request`,
//! `botocore/handlers.py` `add_retry_headers`).
//!
//! `Session::send` builds the URL from the endpoint and the serialized
//! request (`prepare_request_dict`), adds the endpoint headers and the
//! `User-Agent`, then for each attempt signs a fresh copy, adds the
//! `amz-sdk-invocation-id` / `amz-sdk-request` headers and
//! `Content-Length`, sends it, and asks the standard retry policy whether
//! to retry. Retry classification needs the parsed error code of each
//! attempt's response; `send` gets it from `protocol::parse_error`, and
//! `send_with` takes the classifier as a closure.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::cli::GlobalArgs;
use crate::config::{Config, ConfigLookup};
use crate::endpoint::Endpoint;
use crate::model::{Operation, ServiceModel};
use crate::protocol::{HttpRequest, HttpResponse};

pub mod credentials;
pub mod error;
pub mod http;
pub mod imds;
pub mod retry;
pub mod sign;
pub mod url;
pub mod useragent;

pub use error::{ErrorKind, SessionError};

use http::{HttpClient, HttpConfig, Verify};
use retry::{AttemptOutcome, RetryMode, RetryPolicy, RetryState};
use sign::{
    AuthInputs, Credentials, PayloadContext, SigV4, SigV4a, SignableRequest, SignatureVersion,
    headers,
};
use useragent::{Feature, UserAgent};

pub struct Session {
    config: Config,
    http: tokio::sync::OnceCell<Arc<HttpClient>>,
    retry_states: Mutex<HashMap<String, Arc<RetryState>>>,
    credentials: tokio::sync::OnceCell<Option<Credentials>>,
    session_id: String,
    command: Mutex<Option<String>>,
    features: Mutex<Vec<Feature>>,
}

/// The result of sending a request: the final response and how many
/// attempts were made.
pub struct SendOutcome {
    pub response: HttpResponse,
    pub attempts: u32,
    /// True when the last attempt was retryable but the attempt limit was
    /// reached (botocore then appends `(reached max retries: N)`).
    ///
    /// Ported exactly: botocore's `MaxAttemptsChecker` runs first in
    /// `StandardRetryConditions`, so `MaxAttemptsReached` is recorded
    /// whenever the final attempt number equals `max_attempts`, retryable
    /// or not (e.g. `AWS_MAX_ATTEMPTS=1` gives `(reached max retries: 0)`).
    pub max_attempts_reached: bool,
}

/// Client settings resolved when botocore creates a client
/// (`ClientArgsCreator.compute_client_args`, `botocore/config.py`).
#[derive(Debug, Clone, PartialEq)]
pub struct ClientSettings {
    pub region: Option<String>,
    pub retry_mode: RetryMode,
    /// `None` when unset (the retry handler's default applies).
    pub max_attempts: Option<u32>,
    pub connect_timeout: Option<Duration>,
    pub read_timeout: Option<Duration>,
    pub verify: Verify,
}

/// `ServiceId.hyphenize`.
pub fn hyphenize(service_id: &str) -> String {
    service_id.replace(' ', "-").to_lowercase()
}

fn service_id(model: &ServiceModel) -> String {
    hyphenize(model.metadata_str("serviceId").unwrap_or(&model.name))
}

/// `OperationModel.has_streaming_input`: the input's `payload` member is
/// a blob.
pub fn has_streaming_input(operation: &Operation<'_>) -> bool {
    let Some(input) = operation.input() else {
        return false;
    };
    let Some(payload) = input.shape_get("payload").and_then(|p| p.as_str()) else {
        return false;
    };
    input
        .member_shape(payload)
        .is_some_and(|s| s.type_name() == "blob")
}

fn str_list(v: Option<&serde_json::Value>) -> Option<Vec<String>> {
    v?.as_array().map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect()
    })
}

/// `utils.validate_region_name`.
pub fn validate_region_name(region: &str) -> Result<(), SessionError> {
    let ok = region.len() <= 63
        && region
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !region.starts_with('-')
        && !region.ends_with('-')
        && !(!region.is_empty() && region.chars().all(|c| c.is_ascii_digit()));
    if ok {
        Ok(())
    } else {
        Err(SessionError::new(
            ErrorKind::InvalidRegion,
            format!("Provided region_name '{region}' doesn't match a supported format."),
        ))
    }
}

/// `utils.calculate_md5` for a body (`Content-MD5`), for the handlers
/// that add it.
pub fn content_md5(body: &[u8]) -> String {
    use base64::Engine;
    use md5::Digest;
    base64::engine::general_purpose::STANDARD.encode(md5::Md5::digest(body))
}

/// `Endpoint._calculate_ttl`: local time + read timeout + estimated skew
/// from the response `Date` header.
fn calculate_ttl(
    received: DateTime<Utc>,
    date_header: &str,
    read_timeout: Option<Duration>,
) -> Option<String> {
    let read_timeout = read_timeout?;
    let date = chrono::NaiveDateTime::parse_from_str(
        date_header
            .trim_end_matches(" GMT")
            .trim_end_matches(" UTC"),
        "%a, %d %b %Y %H:%M:%S",
    )
    .ok()?
    .and_utc();
    if !(date_header.ends_with(" GMT") || date_header.ends_with(" UTC")) {
        return None;
    }
    let skew = date - received;
    let ttl = Utc::now() + chrono::Duration::from_std(read_timeout).ok()? + skew;
    Some(ttl.format("%Y%m%dT%H%M%SZ").to_string())
}

/// `x-amz-checksum-*` request checksum headers (header mode).
fn checksum_header(hs: &[(String, String)]) -> Option<String> {
    hs.iter()
        .map(|(k, _)| k)
        .find(|k| {
            let l = k.to_lowercase();
            l.starts_with("x-amz-checksum-")
                && l != "x-amz-checksum-mode"
                && l != "x-amz-checksum-type"
        })
        .cloned()
}

impl Session {
    /// Read the configuration: the global timeouts are converted first
    /// (`top-level-args-parsed`), then the config and credentials files
    /// are loaded (`session-initialized`). Errors are `SessionError`s
    /// (convert with `to_cli_error` / downcast from `anyhow::Error`).
    pub fn new(globals: &GlobalArgs) -> Result<Session> {
        Ok(Session::with_env(globals, std::env::vars().collect())?)
    }

    pub fn with_env(
        globals: &GlobalArgs,
        env: HashMap<String, String>,
    ) -> Result<Session, SessionError> {
        for t in [&globals.cli_connect_timeout, &globals.cli_read_timeout]
            .into_iter()
            .flatten()
        {
            error::py_int(t)?;
        }
        let config = Config::with_env(globals, env)?;
        Ok(Session {
            config,
            http: tokio::sync::OnceCell::new(),
            retry_states: Mutex::new(HashMap::new()),
            credentials: tokio::sync::OnceCell::new(),
            session_id: useragent::new_session_id(),
            command: Mutex::new(None),
            features: Mutex::new(Vec::new()),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The resolved region, if any.
    pub fn region(&self) -> Option<String> {
        self.config.region().ok().flatten()
    }

    /// The CLI command names for `md/command#<service>.<operation>`.
    pub fn set_command(&self, cli_service: &str, cli_operation: &str) {
        *self.command.lock().unwrap() = Some(format!("{cli_service}.{cli_operation}"));
    }

    /// Record a user agent feature (e.g. `Feature::Paginator`).
    pub fn add_feature(&self, feature: Feature) {
        self.features.lock().unwrap().push(feature);
    }

    /// The checks botocore runs when creating a client, in its order:
    /// `ProfileNotFound`, the region (`InvalidRegionError`), the retry
    /// configuration (`int()` of `max_attempts`, `InvalidMaxRetryAttemptsError`,
    /// `InvalidRetryModeError`), and the CA bundle.
    pub fn client_settings(&self) -> Result<ClientSettings, SessionError> {
        let region = self.config.region()?;
        self.config.scoped_config()?;
        if let Some(r) = &region {
            validate_region_name(r)?;
        }
        let max_attempts = match self.config.get("max_attempts")? {
            Some(v) => Some(error::py_int(&v)?),
            None => None,
        };
        if let Some(n) = max_attempts
            && n < 1
        {
            return Err(SessionError::new(
                ErrorKind::InvalidConfig,
                format!(
                    "Value provided to \"max_attempts\": {n} must be an integer greater than or equal to one."
                ),
            ));
        }
        let mode = self
            .config
            .get("retry_mode")?
            .unwrap_or_else(|| "standard".to_string());
        let retry_mode = match mode.as_str() {
            "standard" => RetryMode::Standard,
            "adaptive" => RetryMode::Adaptive,
            other => {
                return Err(SessionError::new(
                    ErrorKind::InvalidConfig,
                    format!(
                        "Invalid value provided to \"mode\": \"{other}\" must be one of: \"standard\" or \"adaptive\""
                    ),
                ));
            }
        };
        let timeout = |v: Option<String>| -> Result<Option<Duration>, SessionError> {
            let n = error::py_int(&v.unwrap_or_else(|| "60".to_string()))?;
            Ok((n != 0).then(|| Duration::from_secs(n.max(0) as u64)))
        };
        let connect_timeout = timeout(self.config.get("cli_connect_timeout")?)?;
        let read_timeout = timeout(self.config.get("cli_read_timeout")?)?;
        let verify = if self.config.globals().no_verify_ssl {
            Verify::Off
        } else {
            match self.config.get("ca_bundle")? {
                Some(b) if b.trim().is_empty() => {
                    return Err(SessionError::new(
                        ErrorKind::InvalidConfig,
                        "Invalid CA bundle: the configured value (ca_bundle, AWS_CA_BUNDLE, \
                         REQUESTS_CA_BUNDLE, or verify) resolved to an empty or whitespace-only \
                         string. Provide a valid path to a CA bundle file.",
                    ));
                }
                Some(b) => Verify::Bundle(b),
                None => match self.config.env("REQUESTS_CA_BUNDLE") {
                    Some(b) if b.trim().is_empty() => {
                        return Err(SessionError::new(
                            ErrorKind::InvalidConfig,
                            "Invalid CA bundle: the configured value (ca_bundle, AWS_CA_BUNDLE, \
                             REQUESTS_CA_BUNDLE, or verify) resolved to an empty or whitespace-only \
                             string. Provide a valid path to a CA bundle file.",
                        ));
                    }
                    Some(b) => Verify::Bundle(b.to_string()),
                    None => Verify::Default,
                },
            }
        };
        Ok(ClientSettings {
            region,
            retry_mode,
            max_attempts: max_attempts.map(|n| n as u32),
            connect_timeout,
            read_timeout,
            verify,
        })
    }

    async fn http_client(
        &self,
        settings: &ClientSettings,
    ) -> Result<Arc<HttpClient>, SessionError> {
        self.http
            .get_or_try_init(|| async {
                let forwarding = self
                    .config
                    .get_bool("proxies_config.proxy_use_forwarding_for_https")?
                    .unwrap_or(false);
                Ok(Arc::new(HttpClient::new(HttpConfig {
                    verify: settings.verify.clone(),
                    connect_timeout: settings.connect_timeout,
                    read_timeout: settings.read_timeout,
                    env: self.config.env_map().clone(),
                    proxy_use_forwarding_for_https: forwarding,
                })?))
            })
            .await
            .cloned()
    }

    fn retry_state(&self, service: &str, mode: RetryMode) -> Arc<RetryState> {
        self.retry_states
            .lock()
            .unwrap()
            .entry(service.to_string())
            .or_insert_with(|| Arc::new(RetryState::new(mode)))
            .clone()
    }

    /// The credentials (resolved once per session); `None` when no
    /// provider has any.
    pub async fn credentials(
        &self,
        region: Option<&str>,
    ) -> Result<Option<Credentials>, SessionError> {
        self.credentials
            .get_or_try_init(|| credentials::resolve_credentials(&self.config, region))
            .await
            .cloned()
    }

    fn user_agent(&self, settings: &ClientSettings) -> String {
        let mut features = self.features.lock().unwrap().clone();
        features.push(match settings.retry_mode {
            RetryMode::Standard => Feature::RetryModeStandard,
            RetryMode::Adaptive => Feature::RetryModeAdaptive,
        });
        if self.config.globals().endpoint_url.is_some() {
            features.push(Feature::EndpointOverride);
        }
        if self.config.env("CLAUDECODE").is_some_and(|v| !v.is_empty()) {
            features.push(Feature::AgenticCallerClaudeCode);
        }
        if self
            .config
            .config_variable("request_checksum_calculation")
            .as_deref()
            == Some("when_supported")
        {
            features.push(Feature::FlexibleChecksumsReqWhenSupported);
        }
        if self
            .config
            .config_variable("response_checksum_validation")
            .as_deref()
            == Some("when_supported")
        {
            features.push(Feature::FlexibleChecksumsResWhenSupported);
        }
        if !self.config.profile_explicitly_set()
            && credentials::env_credentials(&self.config)
                .ok()
                .flatten()
                .is_some()
        {
            features.push(Feature::CredentialsEnvVars);
        }
        UserAgent {
            cli_version: crate::model::REFERENCE_VERSION.trim().to_string(),
            execution_env: self.config.env("AWS_EXECUTION_ENV").map(str::to_string),
            features,
            retry_mode: Some(settings.retry_mode.as_str().to_string()),
            app_id: self.config.config_variable("user_agent_appid"),
            session_id: self.session_id.clone(),
            command: self.command.lock().unwrap().clone(),
        }
        .to_header()
    }

    /// The request URL `prepare_request_dict` builds for `request` at
    /// `endpoint` (host prefix dropped when
    /// `disable_host_prefix_injection` is true).
    pub fn request_url(
        &self,
        endpoint: &Endpoint,
        request: &HttpRequest,
    ) -> Result<String, SessionError> {
        let inject = self.config.get_bool("disable_host_prefix_injection")? != Some(true);
        let prefix = if inject {
            request.host_prefix.as_deref()
        } else {
            None
        };
        Ok(url::request_url(
            &endpoint.url,
            &request.url_path,
            &request.query,
            prefix,
        ))
    }

    /// Sign (per the operation's auth types and the endpoint's
    /// `authSchemes`) and send `request` to `endpoint`, retrying per the
    /// configured retry mode. Each attempt's error code for retry
    /// classification comes from `protocol::parse_error` when the status
    /// is 300 or more.
    pub async fn send(
        &self,
        model: &ServiceModel,
        operation: &Operation<'_>,
        endpoint: &Endpoint,
        request: &HttpRequest,
    ) -> Result<SendOutcome> {
        let classify = |resp: &HttpResponse| -> Option<String> {
            if resp.status < 300 {
                return None;
            }
            crate::protocol::parse_error(model, operation, resp, &Default::default())
                .ok()
                .map(|e| e.code)
                .filter(|c| !c.is_empty())
        };
        self.send_with(model, operation, endpoint, request, &classify)
            .await
    }

    /// `send` with an explicit classifier returning botocore's
    /// `parsed_response['Error']['Code']` for an attempt's response
    /// (`None` for success or when there is none). Errors are
    /// `SessionError`s wrapped in `anyhow::Error`.
    pub async fn send_with(
        &self,
        model: &ServiceModel,
        operation: &Operation<'_>,
        endpoint: &Endpoint,
        request: &HttpRequest,
        classify: &(dyn Fn(&HttpResponse) -> Option<String> + Sync),
    ) -> Result<SendOutcome> {
        Ok(self
            .send_inner(model, operation, endpoint, request, classify)
            .await?)
    }

    async fn send_inner(
        &self,
        model: &ServiceModel,
        operation: &Operation<'_>,
        endpoint: &Endpoint,
        request: &HttpRequest,
        classify: &(dyn Fn(&HttpResponse) -> Option<String> + Sync),
    ) -> Result<SendOutcome, SessionError> {
        let settings = self.client_settings()?;
        let sid = service_id(model);
        let new_retries = retry::new_retries_enabled(self.config.env_map());
        let policy = RetryPolicy {
            max_attempts: RetryState::max_attempts(settings.max_attempts, &sid, new_retries),
            service_name: model.name.clone(),
            service_id: sid.clone(),
            operation_name: operation.name.to_string(),
            modeled: retry::modeled_retryable_errors(operation),
            new_retries,
        };
        let state = self.retry_state(&model.name, settings.retry_mode);
        let http = self.http_client(&settings).await?;

        // Signer selection.
        let scoped_signature_version = self
            .config
            .scoped_config()?
            .get(model.endpoint_prefix())
            .and_then(|v| v.as_nested())
            .and_then(|m| m.get("signature_version"))
            .filter(|v| !v.is_empty())
            .cloned();
        let auth_inputs = AuthInputs {
            service_name: &model.name,
            endpoint_prefix: model.endpoint_prefix(),
            signing_name: model.signing_name(),
            service_id: &sid,
            operation_name: operation.name,
            service_auth: str_list(model.metadata().get("auth")),
            service_signature_version: model.metadata_str("signatureVersion").map(str::to_string),
            operation_auth: str_list(operation.get("auth")),
            operation_authtype: operation
                .get("authtype")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            unsigned_payload: operation
                .get("unsignedPayload")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            auth_schemes: endpoint.properties.get("authSchemes"),
            no_sign_request: self.config.globals().no_sign_request,
            configured_signature_version: scoped_signature_version,
            auth_scheme_preference: self.config.get("auth_scheme_preference")?,
            sigv4a_signing_region_set: self.config.get("sigv4a_signing_region_set")?,
            region: settings.region.clone(),
        };
        let mut choice = sign::choose_signer(&auth_inputs)?;
        // `_set_auth_scheme_preference_signer` prefers a bearer token from
        // the environment for the services that allow it.
        if !matches!(
            choice.version,
            SignatureVersion::Unsigned | SignatureVersion::Bearer
        ) && credentials::env_bearer_token(&self.config, &choice.signing_name).is_some()
            && auth_inputs
                .service_auth
                .as_ref()
                .is_some_and(|a| a.iter().any(|x| x == "smithy.api#httpBearerAuth"))
            && auth_inputs.operation_auth.is_none()
        {
            choice.version = SignatureVersion::Bearer;
        }
        let s3_payload_signing = self.config.get_bool("s3.payload_signing_enabled")?;

        let url = self.request_url(endpoint, request)?;
        let mut base_headers = request.headers.clone();
        for (k, v) in &endpoint.headers {
            if let Some(first) = v.first() {
                headers::set(&mut base_headers, k, first);
            }
        }
        headers::set(&mut base_headers, "User-Agent", &self.user_agent(&settings));

        let ctx = PayloadContext {
            payload_signing_enabled: choice.payload_signing_enabled,
            has_streaming_input: has_streaming_input(operation),
            s3_payload_signing_enabled: s3_payload_signing,
            checksum_in_trailer: headers::contains(&base_headers, "x-amz-trailer"),
            checksum_header: checksum_header(&base_headers),
        };

        let invocation_id = uuid::Uuid::new_v4().to_string();
        let mut ttl: Option<String> = None;
        let mut max_known = false;
        let mut quota_capacity: Option<u32> = None;
        let mut attempt: u32 = 1;
        loop {
            let mut req = SignableRequest {
                method: request.method.clone(),
                url: url.clone(),
                headers: base_headers.clone(),
                body: request.body.clone(),
            };
            self.sign(&mut req, &choice, &ctx, settings.region.as_deref())
                .await?;
            // `add_retry_headers`.
            headers::set(&mut req.headers, "amz-sdk-invocation-id", &invocation_id);
            let mut parts = Vec::new();
            if let Some(t) = &ttl {
                parts.push(format!("ttl={t}"));
            }
            parts.push(format!("attempt={attempt}"));
            if max_known {
                parts.push(format!("max={}", policy.max_attempts));
            }
            headers::set(&mut req.headers, "amz-sdk-request", &parts.join("; "));
            // `AWSRequestPreparer._prepare_headers`.
            if !headers::contains(&req.headers, "Transfer-Encoding")
                && !headers::contains(&req.headers, "Content-Length")
                && !matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS")
            {
                req.headers
                    .push(("Content-Length".to_string(), req.body.len().to_string()));
            }
            if let Some(wait) = state.before_send() {
                tokio::time::sleep(wait).await;
            }
            let result = http
                .send(&req.method, &req.url, &req.headers, &req.body)
                .await;
            let received = Utc::now();
            let (response, exception) = match result {
                Ok(r) => (Some(r), None),
                Err(e) => (None, Some(e)),
            };
            let error_code = response.as_ref().and_then(classify);
            let outcome = AttemptOutcome {
                attempt,
                response: response.as_ref(),
                error_code: error_code.as_deref(),
                exception: exception.as_ref(),
            };
            let decision = state.needs_retry(&policy, &outcome, &mut quota_capacity);
            max_known = true;
            match decision.delay {
                Some(delay) => {
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    if let Some(r) = &response
                        && let Some(date) = r.header("Date")
                    {
                        ttl = calculate_ttl(received, date, settings.read_timeout).or(ttl);
                    }
                }
                None => {
                    return match (response, exception) {
                        (Some(response), _) => {
                            state.release(response.status, quota_capacity);
                            Ok(SendOutcome {
                                response,
                                attempts: attempt,
                                max_attempts_reached: decision.max_attempts_reached,
                            })
                        }
                        (None, Some(e)) => Err(e),
                        (None, None) => unreachable!(),
                    };
                }
            }
        }
    }

    async fn sign(
        &self,
        req: &mut SignableRequest,
        choice: &sign::SignerChoice,
        ctx: &PayloadContext,
        client_region: Option<&str>,
    ) -> Result<(), SessionError> {
        match &choice.version {
            SignatureVersion::Unsigned => {
                if ctx.checksum_in_trailer {
                    headers::set(
                        &mut req.headers,
                        "X-Amz-Content-SHA256",
                        sign::STREAMING_UNSIGNED_PAYLOAD_TRAILER,
                    );
                }
                Ok(())
            }
            SignatureVersion::Bearer => {
                let token =
                    credentials::resolve_token(&self.config, &choice.signing_name, client_region)
                        .await?
                        .ok_or_else(|| {
                            SessionError::new(
                                ErrorKind::NoAuthToken,
                                "Unable to locate authorization token",
                            )
                        })?;
                headers::replace(
                    &mut req.headers,
                    "Authorization",
                    &format!("Bearer {token}"),
                );
                Ok(())
            }
            version => {
                if client_region.is_none() {
                    return Err(SessionError::new(
                        ErrorKind::NoRegion,
                        "You must specify a region.",
                    ));
                }
                let creds = self
                    .credentials(client_region)
                    .await?
                    .ok_or_else(SessionError::no_credentials)?;
                let region = choice.region.clone().unwrap_or_default();
                match version {
                    SignatureVersion::V4a { s3 } => SigV4a {
                        credentials: creds,
                        service_name: choice.signing_name.clone(),
                        region_set: region,
                        s3: *s3,
                    }
                    .add_auth(req, ctx, Utc::now()),
                    SignatureVersion::V4 { s3 } => {
                        SigV4 {
                            credentials: creds,
                            service_name: choice.signing_name.clone(),
                            region_name: region,
                            s3: *s3,
                        }
                        .add_auth(req, ctx, Utc::now());
                        Ok(())
                    }
                    SignatureVersion::Other(name) => {
                        SigV4 {
                            credentials: creds,
                            service_name: choice.signing_name.clone(),
                            region_name: region,
                            s3: name.starts_with("s3") || name.contains("s3express"),
                        }
                        .add_auth(req, ctx, Utc::now());
                        Ok(())
                    }
                    SignatureVersion::Unsigned | SignatureVersion::Bearer => unreachable!(),
                }
            }
        }
    }
}

impl ConfigLookup for Session {
    fn config_variable(&self, name: &str) -> Option<String> {
        self.config.config_variable(name)
    }
}

#[cfg(test)]
mod tests;
