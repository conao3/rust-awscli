//! The clients the credential providers and the `configure` / `sso` /
//! `login` commands create internally (`Session.create_client` for
//! `sts`, `sso`, `sso-oidc`, and `signin`): serialize, resolve the
//! endpoint, sign with explicit credentials (or not at all), send, and
//! parse, with the CLI's timestamp parser (`cli_timestamp_format`
//! `iso8601`).

use chrono::Utc;

use super::error::{ErrorKind, SessionError};
use super::http::{HttpClient, HttpConfig, Verify};
use super::sign::{
    AuthInputs, Credentials, PayloadContext, SigV4, SignableRequest, SignatureVersion, headers,
};
use crate::config::Config;
use crate::endpoint::{self, BuiltinInputs, Builtins, S3Config};
use crate::model::ServiceModel;
use crate::protocol::{self, ParseOptions};
use crate::value::Value;

/// A failed call: `botocore.exceptions.ClientError` or an exception raised
/// before a response was parsed.
#[derive(Debug, Clone)]
pub enum ApiError {
    Session(SessionError),
    Service(Box<ServiceFailure>),
}

#[derive(Debug, Clone)]
pub struct ServiceFailure {
    pub operation: String,
    pub code: String,
    pub message: String,
    /// The parsed error response (`e.response`).
    pub parsed: Value,
}

impl ServiceFailure {
    /// `str(ClientError)`.
    pub fn client_error_message(&self) -> String {
        format!(
            "An error occurred ({}) when calling the {} operation: {}",
            self.code, self.operation, self.message
        )
    }
}

impl ApiError {
    /// The exception as the CLI's handlers render it: a `ClientError`
    /// exits 254 with its `str()`.
    pub fn into_session_error(self) -> SessionError {
        match self {
            ApiError::Session(e) => e,
            ApiError::Service(f) => {
                SessionError::new(ErrorKind::ServiceError, f.client_error_message())
            }
        }
    }

    pub fn code(&self) -> Option<&str> {
        match self {
            ApiError::Service(f) => Some(&f.code),
            ApiError::Session(_) => None,
        }
    }
}

impl From<SessionError> for ApiError {
    fn from(e: SessionError) -> ApiError {
        ApiError::Session(e)
    }
}

fn general(e: impl std::fmt::Display) -> ApiError {
    ApiError::Session(SessionError::new(ErrorKind::General, e.to_string()))
}

/// Headers computed from the final request URL (a `before-call`
/// handler such as the DPoP proof).
pub type HeaderHook<'a> = &'a (dyn Fn(&str) -> Vec<(String, String)> + Sync);

/// An internal client for one service.
pub struct ApiClient<'a> {
    config: &'a Config,
    model: ServiceModel,
    region: Option<String>,
    endpoint_url: Option<String>,
    credentials: Option<Credentials>,
    verify: Verify,
}

impl<'a> ApiClient<'a> {
    /// `session.create_client(service, region_name=region, ...)`, unsigned
    /// unless `credentials` are given. The configured endpoint URL
    /// (`AWS_ENDPOINT_URL_<SERVICE>`, `AWS_ENDPOINT_URL`, `[services]`,
    /// `endpoint_url`) applies unless `endpoint_url` is given.
    pub fn new(
        config: &'a Config,
        service: &str,
        region: Option<String>,
        endpoint_url: Option<String>,
        credentials: Option<Credentials>,
    ) -> Result<ApiClient<'a>, SessionError> {
        let mut model = ServiceModel::load(service)
            .map_err(|e| SessionError::new(ErrorKind::General, e.to_string()))?;
        model.apply_endpoint_ruleset_customizations();
        let endpoint_url = match endpoint_url {
            Some(u) => Some(u),
            None => configured_endpoint_url(config, service)?,
        };
        let verify = if config.globals().no_verify_ssl {
            Verify::Off
        } else {
            match config.get("ca_bundle").ok().flatten() {
                Some(b) => Verify::Bundle(b),
                None => match config.env("REQUESTS_CA_BUNDLE") {
                    Some(b) => Verify::Bundle(b.to_string()),
                    None => Verify::Default,
                },
            }
        };
        Ok(ApiClient {
            config,
            model,
            region,
            endpoint_url,
            credentials,
            verify,
        })
    }

    fn builtins(&self) -> Builtins {
        let config = self.config;
        Builtins::compute(&BuiltinInputs {
            region_name: self.region.clone(),
            endpoint_prefix: self.model.endpoint_prefix().to_string(),
            s3_config: S3Config::default(),
            use_fips_endpoint: config.get_bool("use_fips_endpoint").ok().flatten(),
            use_dualstack_endpoint: config.get_bool("use_dualstack_endpoint").ok().flatten(),
            endpoint_url: self.endpoint_url.clone(),
            account_id: None,
            account_id_endpoint_mode: None,
            s3_disable_express_session_auth: None,
            client_context_params: Default::default(),
        })
    }

    /// The URL a call of `operation` with `params` is sent to (the
    /// `before-call` endpoint extraction of `get_base_sign_in_uri` and
    /// `_get_base_authorization_uri`).
    pub fn request_url(&self, operation: &str, params: &Value) -> Result<String, ApiError> {
        let op = self
            .model
            .operation(operation)
            .ok_or_else(|| general(format!("unknown operation {operation}")))?;
        let endpoint =
            endpoint::construct_endpoint(&self.model, &op, params, &self.builtins(), true)
                .map_err(general)?;
        let request = protocol::serialize(&self.model, &op, params).map_err(general)?;
        Ok(super::url::request_url(
            &endpoint.url,
            &request.url_path,
            &request.query,
            request.host_prefix.as_deref(),
        ))
    }

    /// Call `operation`. `extra_headers` computes headers from the final
    /// URL (the DPoP `before-call` handler).
    pub async fn call(
        &self,
        operation: &str,
        params: &Value,
        extra_headers: Option<HeaderHook<'_>>,
    ) -> Result<Value, ApiError> {
        let model = &self.model;
        let op = model
            .operation(operation)
            .ok_or_else(|| general(format!("unknown operation {operation}")))?;
        if let Some(input) = op.input()
            && let Some(report) = crate::cli::validate::validate(params, &input)
        {
            return Err(ApiError::Session(SessionError::new(
                ErrorKind::ParamValidation,
                report,
            )));
        }
        let endpoint = endpoint::construct_endpoint(model, &op, params, &self.builtins(), true)
            .map_err(general)?;
        let request = protocol::serialize(model, &op, params).map_err(general)?;
        let url = super::url::request_url(
            &endpoint.url,
            &request.url_path,
            &request.query,
            request.host_prefix.as_deref(),
        );
        let mut hdrs = request.headers.clone();
        for (k, v) in &endpoint.headers {
            if let Some(first) = v.first() {
                headers::set(&mut hdrs, k, first);
            }
        }
        headers::set(
            &mut hdrs,
            "User-Agent",
            &format!("aws-cli/{} md/awsr", crate::model::REFERENCE_VERSION.trim()),
        );
        if let Some(f) = extra_headers {
            for (k, v) in f(&url) {
                headers::set(&mut hdrs, &k, &v);
            }
        }
        let mut req = SignableRequest {
            method: request.method.clone(),
            url,
            headers: hdrs,
            body: request.body.clone(),
        };
        if let Some(creds) = &self.credentials {
            let sid = super::hyphenize(model.metadata_str("serviceId").unwrap_or(&model.name));
            let str_list = |v: Option<&serde_json::Value>| -> Option<Vec<String>> {
                v?.as_array().map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
            };
            let inputs = AuthInputs {
                service_name: &model.name,
                endpoint_prefix: model.endpoint_prefix(),
                signing_name: model.signing_name(),
                service_id: &sid,
                operation_name: op.name,
                service_auth: str_list(model.metadata().get("auth")),
                service_signature_version: model
                    .metadata_str("signatureVersion")
                    .map(str::to_string),
                operation_auth: str_list(op.get("auth")),
                operation_authtype: op
                    .get("authtype")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                unsigned_payload: false,
                auth_schemes: endpoint.properties.get("authSchemes"),
                region: self.region.clone(),
                ..Default::default()
            };
            let choice = super::sign::choose_signer(&inputs)?;
            if !matches!(
                choice.version,
                SignatureVersion::Unsigned | SignatureVersion::Bearer
            ) {
                SigV4 {
                    credentials: creds.clone(),
                    service_name: choice.signing_name.clone(),
                    region_name: choice.region.clone().unwrap_or_default(),
                    s3: false,
                }
                .add_auth(&mut req, &PayloadContext::default(), Utc::now());
            }
        }
        if !headers::contains(&req.headers, "Content-Length")
            && !matches!(req.method.as_str(), "GET" | "HEAD" | "OPTIONS")
        {
            req.headers
                .push(("Content-Length".to_string(), req.body.len().to_string()));
        }
        let http = HttpClient::new(HttpConfig {
            verify: self.verify.clone(),
            connect_timeout: Some(std::time::Duration::from_secs(60)),
            read_timeout: Some(std::time::Duration::from_secs(60)),
            env: self.config.env_map().clone(),
            proxy_use_forwarding_for_https: false,
        })?;
        let response = http
            .send(&req.method, &req.url, &req.headers, &req.body)
            .await?;
        let options = ParseOptions::default();
        if response.status >= 300 {
            let error = protocol::parse_error(model, &op, &response, &options).map_err(general)?;
            let code = if error.code.is_empty() {
                "Unknown".to_string()
            } else {
                error.code.clone()
            };
            let message = error
                .parsed
                .get("Error")
                .and_then(|e| e.get("Message"))
                .and_then(Value::as_str)
                .unwrap_or("Unknown")
                .to_string();
            return Err(ApiError::Service(Box::new(ServiceFailure {
                operation: op.name.to_string(),
                code,
                message,
                parsed: error.parsed,
            })));
        }
        protocol::parse(model, &op, &response, &options).map_err(general)
    }
}

/// `ConfiguredEndpointProvider.provide()` for an internal client; a
/// missing profile reads only the environment.
fn configured_endpoint_url(config: &Config, service: &str) -> Result<Option<String>, SessionError> {
    match config.configured_endpoint_url(service) {
        Ok(u) => Ok(u),
        Err(e) if e.kind == ErrorKind::ProfileNotFound => {
            if config
                .get_bool("ignore_configured_endpoint_urls")
                .ok()
                .flatten()
                == Some(true)
            {
                return Ok(None);
            }
            let id = crate::config::snake_case_service_id(service).to_uppercase();
            Ok(config
                .env(&format!("AWS_ENDPOINT_URL_{id}"))
                .or_else(|| config.env("AWS_ENDPOINT_URL"))
                .filter(|u| !u.is_empty())
                .map(str::to_string))
        }
        Err(e) => Err(e),
    }
}

/// `botocore.utils.percent_encode_sequence` for string pairs.
pub fn percent_encode_sequence(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `botocore.utils.percent_encode` with `safe='-._~'`.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
