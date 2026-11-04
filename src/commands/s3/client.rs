//! The botocore S3 client the high-level commands use: every call goes
//! through [`Caller::call`], so endpoints, signing, handlers, and retries
//! are the ones of `s3api`.

use std::fmt;

use crate::driver::{CallResult, Caller, ClientConfig, Context};
use crate::model::ServiceModel;
use crate::output::errors::ClientErrorInfo;
use crate::value::{Map, Value};

/// What a client call raises.
#[derive(Debug)]
pub(crate) enum CallError {
    /// `ClientError`.
    Service(Box<ClientErrorInfo>),
    /// `botocore.exceptions.ParamValidationError`.
    Validation(String),
    /// Any other exception (connection errors, ...).
    Other(anyhow::Error),
}

impl CallError {
    pub(crate) fn code(&self) -> Option<&str> {
        match self {
            CallError::Service(i) => Some(&i.code),
            CallError::Validation(_) | CallError::Other(_) => None,
        }
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::Service(i) => f.write_str(&i.exception_message()),
            CallError::Validation(m) => f.write_str(m),
            CallError::Other(e) => write!(f, "{e}"),
        }
    }
}

pub(crate) struct Client<'a> {
    pub ctx: &'a Context,
    pub model: &'static ServiceModel,
    pub config: ClientConfig,
}

/// The S3 model with the endpoint rule set customizations applied,
/// loaded once per process.
pub(crate) fn s3_model() -> anyhow::Result<&'static ServiceModel> {
    static MODEL: std::sync::OnceLock<ServiceModel> = std::sync::OnceLock::new();
    if let Some(m) = MODEL.get() {
        return Ok(m);
    }
    let mut m = ServiceModel::load("s3")?;
    m.apply_endpoint_ruleset_customizations();
    Ok(MODEL.get_or_init(|| m))
}

impl<'a> Client<'a> {
    pub(crate) fn new(ctx: &'a Context) -> anyhow::Result<Client<'a>> {
        Client::with_config(ctx, ClientConfig::default())
    }

    /// `session.create_client('s3', region_name=..., endpoint_url=...)`.
    pub(crate) fn with_config(
        ctx: &'a Context,
        config: ClientConfig,
    ) -> anyhow::Result<Client<'a>> {
        Ok(Client {
            ctx,
            model: s3_model()?,
            config,
        })
    }

    /// `client.<operation>(**params)`, with `None` values dropped.
    pub(crate) fn call(&self, operation: &str, params: Map) -> Result<Map, CallError> {
        let params: Map = params
            .into_iter()
            .filter(|(_, v)| !matches!(v, Value::Null))
            .collect();
        let params = Value::Map(params);
        let caller = Caller::with_config(self.ctx, self.model, operation, self.config.clone())
            .map_err(CallError::Other)?;
        let raised =
            |e: anyhow::Error| match e.downcast_ref::<crate::handlers::ParamValidationError>() {
                Some(p) => CallError::Validation(p.to_string()),
                None => CallError::Other(e),
            };
        match crate::handlers::with_bytes_payload(|| caller.call(&params)).map_err(raised)? {
            CallResult::Ok(Value::Map(m)) => Ok(m
                .into_iter()
                .filter(|(_, v)| !matches!(v, Value::Null))
                .collect()),
            CallResult::Ok(_) => Ok(Map::new()),
            CallResult::ServiceError(info) => Err(CallError::Service(info)),
        }
    }

    /// `ParamValidator` against the operation input shape.
    pub(crate) fn validate(&self, operation: &str, params: &Value) -> Result<(), CallError> {
        let op = self
            .model
            .operation(operation)
            .ok_or_else(|| CallError::Other(anyhow::anyhow!("unknown operation {operation}")))?;
        if let Some(input) = op.input()
            && let Some(msg) = crate::cli::validate::validate(params, &input)
        {
            return Err(CallError::Validation(msg));
        }
        Ok(())
    }

    /// The pages of a `ListObjectsV2` paginator (`PageSize` becomes
    /// `MaxKeys`), handed to `f` one at a time.
    pub(crate) fn list_objects_v2_pages(
        &self,
        mut params: Map,
        page_size: Option<i64>,
        f: &mut dyn FnMut(Map) -> Result<bool, CallError>,
    ) -> Result<(), CallError> {
        if let Some(n) = page_size {
            params.insert("MaxKeys".into(), Value::int(n));
        }
        let mut seen: Vec<String> = vec![];
        loop {
            let page = self.call("ListObjectsV2", params.clone())?;
            let next = page
                .get("NextContinuationToken")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            if !f(page)? {
                return Ok(());
            }
            let Some(token) = next else {
                return Ok(());
            };
            if seen.contains(&token) {
                return Err(CallError::Other(anyhow::anyhow!(
                    "The same next token was received twice: {{'ContinuationToken': '{token}'}}"
                )));
            }
            seen.push(token.clone());
            params.insert("ContinuationToken".into(), Value::Str(token));
        }
    }
}

/// Build a map from pairs.
pub(crate) fn map(pairs: &[(&str, Value)]) -> Map {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    m
}
