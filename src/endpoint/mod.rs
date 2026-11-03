//! Endpoint resolution: an interpreter for `endpoint-rule-set-1.json` with
//! the standard library and `partitions.json`
//! (`botocore/endpoint_provider.py`, `botocore/regions.py`).

mod pyurl;
mod stdlib;

use std::sync::OnceLock;

use anyhow::Result;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::Value as Json;

use crate::model::{Operation, ServiceModel};
use crate::value::Value;
use stdlib::{Scope, Val};

/// An endpoint rule set parameter value.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    String(String),
    Bool(bool),
    StringArray(Vec<String>),
}

impl ParamValue {
    fn to_val(&self) -> Val {
        match self {
            ParamValue::String(s) => Val::Str(s.clone()),
            ParamValue::Bool(b) => Val::Bool(*b),
            ParamValue::StringArray(a) => Val::Tuple(a.iter().cloned().map(Val::Str).collect()),
        }
    }

    /// Python `str(value)` of the value botocore passes to the provider
    /// (string arrays are tuples).
    pub fn py_str(&self) -> String {
        self.to_val().py_str()
    }

    /// Convert a parameter / JMESPath value the way botocore hands it to
    /// the provider (`list` becomes a `tuple`).
    fn from_value(v: &Value) -> Option<ParamValue> {
        match v {
            Value::Null => None,
            Value::Bool(b) => Some(ParamValue::Bool(*b)),
            Value::Str(s) => Some(ParamValue::String(s.clone())),
            Value::List(l) => Some(ParamValue::StringArray(
                l.iter().map(crate::pyfmt::py_str).collect(),
            )),
            other => Some(ParamValue::String(crate::pyfmt::py_str(other))),
        }
    }

    fn from_json(v: &Json) -> Option<ParamValue> {
        ParamValue::from_value(&Value::from_json(v))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub url: String,
    /// `endpoint.properties` as JSON (e.g. `authSchemes`).
    pub properties: serde_json::Map<String, serde_json::Value>,
    /// `endpoint.headers` (each header may have several values).
    pub headers: IndexMap<String, Vec<String>>,
}

/// The exceptions endpoint resolution raises, with `Display` equal to
/// Python's `str(exception)`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EndpointError {
    /// `EndpointResolutionError` (`fmt = '{msg}'`), including error rules.
    #[error("{0}")]
    Resolution(String),
    /// A non-botocore Python exception (`TypeError`, `KeyError`, ...)
    /// raised while evaluating; the message is `str(exception)`.
    #[error("{0}")]
    Internal(String),
    /// `UnknownEndpointResolutionBuiltInName`.
    #[error("Unknown builtin variable name: {0}")]
    UnknownBuiltIn(String),
    /// `InvalidHostLabelError`.
    #[error("Invalid host label to be bound to the hostname of the endpoint: \"{0}\".")]
    InvalidHostLabel(String),
    /// `UnsupportedS3ConfigurationError`.
    #[error("Unsupported configuration when using S3: {0}")]
    UnsupportedS3Configuration(String),
    /// `UnsupportedS3AccesspointConfigurationError`.
    #[error("Unsupported configuration when using S3 access-points: {0}")]
    UnsupportedS3AccesspointConfiguration(String),
    /// botocore `ParamValidationError`.
    #[error("Parameter validation failed:\n{0}")]
    ParamValidation(String),
    /// `UnsupportedS3ControlArnError` (`arn` is `None` when no `Bucket`).
    #[error("S3 ARN \"{}\" provided is invalid for this operation. {msg}", arn.as_deref().unwrap_or("None"))]
    UnsupportedS3ControlArn { arn: Option<String>, msg: String },
    /// `UnsupportedS3ControlConfigurationError`.
    #[error("Unsupported configuration when using S3 Control: {0}")]
    UnsupportedS3ControlConfiguration(String),
    /// `InvalidEndpointConfigurationError`.
    #[error("Invalid endpoint configuration: {0}")]
    InvalidEndpointConfiguration(String),
    /// `UnknownSignatureVersionError`.
    #[error("Unknown Signature Version: {0}.")]
    UnknownSignatureVersion(String),
    /// `UnsupportedSignatureVersionError`.
    #[error("Signature version(s) are not supported: {0}")]
    UnsupportedSignatureVersion(String),
}

impl EndpointError {
    /// The AWS CLI exit status for this error (`awscli/errorhandler.py`):
    /// botocore `ParamValidationError` goes to `ParamValidationErrorsHandler`
    /// (252), everything else to `GeneralExceptionHandler` (255).
    pub fn exit_code(&self) -> i32 {
        match self {
            EndpointError::ParamValidation(_) => 252,
            _ => 255,
        }
    }
}

/// Evaluate `ruleset` with `params`. An error rule becomes an `Err` whose
/// message is the rule's error string. The error is an [`EndpointError`].
///
/// Ports `EndpointProvider.resolve_endpoint` / `RuleSet.evaluate`.
pub fn resolve(
    ruleset: &serde_json::Value,
    params: &IndexMap<String, ParamValue>,
) -> Result<Endpoint> {
    Ok(resolve_endpoint(ruleset, params)?)
}

/// [`resolve`] with the typed error.
pub fn resolve_endpoint(
    ruleset: &Json,
    params: &IndexMap<String, ParamValue>,
) -> Result<Endpoint, EndpointError> {
    let mut scope: Scope = params
        .iter()
        .map(|(k, v)| (k.clone(), v.to_val()))
        .collect();
    process_input_parameters(ruleset, &mut scope)?;
    let rules = ruleset
        .get("rules")
        .and_then(Json::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for rule in rules {
        if let Some(endpoint) = evaluate_rule(rule, scope.clone())? {
            return Ok(endpoint);
        }
    }
    let param_string = params
        .iter()
        .map(|(k, v)| format!("{k}: {}", v.py_str()))
        .collect::<Vec<_>>()
        .join("\n");
    Err(EndpointError::Resolution(format!(
        "No endpoint found for parameters:\n{param_string}"
    )))
}

/// `RuleSet.process_input_parameters` with
/// `ParameterDefinition.process_input` / `validate_input`.
fn process_input_parameters(ruleset: &Json, scope: &mut Scope) -> Result<(), EndpointError> {
    let Some(specs) = ruleset.get("parameters").and_then(Json::as_object) else {
        return Ok(());
    };
    for (name, spec) in specs {
        let ty = spec.get("type").and_then(Json::as_str).unwrap_or_default();
        let (type_repr, ok): (&str, fn(&Val) -> bool) = match ty.to_lowercase().as_str() {
            "string" => ("<class 'str'>", |v| matches!(v, Val::Str(_))),
            "boolean" => ("<class 'bool'>", |v| matches!(v, Val::Bool(_))),
            "stringarray" => ("<class 'tuple'>", |v| matches!(v, Val::Tuple(_))),
            _ => {
                return Err(EndpointError::Resolution(format!(
                    "Unknown parameter type: {ty}. A parameter must be of type string, boolean, or stringarray."
                )));
            }
        };
        match scope.get(name) {
            None | Some(Val::None) => {
                let default = spec.get("default").unwrap_or(&Json::Null);
                if !default.is_null() {
                    scope.insert(name.clone(), Val::from_json(default));
                } else if spec.get("required").and_then(Json::as_bool) == Some(true) {
                    return Err(EndpointError::Internal(
                        "BotoCoreError.__init__() takes 1 positional argument but 2 were given"
                            .into(),
                    ));
                }
            }
            Some(v) => {
                if !ok(v) {
                    return Err(EndpointError::Resolution(format!(
                        "Value ({name}) is the wrong type. Must be {type_repr}."
                    )));
                }
            }
        }
    }
    Ok(())
}

/// `TreeRule` / `EndpointRule` / `ErrorRule` `.evaluate`.
fn evaluate_rule(rule: &Json, mut scope: Scope) -> Result<Option<Endpoint>, EndpointError> {
    let rule_type = rule.get("type").and_then(Json::as_str).unwrap_or_default();
    if !matches!(rule_type, "endpoint" | "error" | "tree") {
        return Err(EndpointError::Resolution(format!(
            "Unknown rule type: {rule_type}. A rule must be of type tree, endpoint or error."
        )));
    }
    if let Some(conditions) = rule.get("conditions").and_then(Json::as_array) {
        for condition in conditions {
            let sig = condition.as_object().ok_or_else(|| {
                EndpointError::Internal("string indices must be integers, not 'str'".into())
            })?;
            let result = stdlib::call_function(sig, &mut scope)?;
            if matches!(result, Val::Bool(false) | Val::None) {
                return Ok(None);
            }
        }
    }
    match rule_type {
        "endpoint" => {
            let endpoint = &rule["endpoint"];
            let url = stdlib::resolve_value(&endpoint["url"], &mut scope)?.py_str();
            let properties = match endpoint.get("properties") {
                Some(p) => resolve_properties(p, &scope)?,
                None => Json::Object(Default::default()),
            };
            let properties = match properties {
                Json::Object(o) => o,
                _ => Default::default(),
            };
            let mut headers = IndexMap::new();
            if let Some(h) = endpoint.get("headers").and_then(Json::as_object) {
                for (name, values) in h {
                    let mut resolved = Vec::new();
                    for item in values.as_array().map(Vec::as_slice).unwrap_or_default() {
                        resolved.push(stdlib::resolve_value(item, &mut scope)?.py_str());
                    }
                    headers.insert(name.clone(), resolved);
                }
            }
            Ok(Some(Endpoint {
                url,
                properties,
                headers,
            }))
        }
        "error" => {
            let msg = stdlib::resolve_value(&rule["error"], &mut scope)?.py_str();
            Err(EndpointError::Resolution(msg))
        }
        _ => {
            for sub in rule
                .get("rules")
                .and_then(Json::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                if let Some(endpoint) = evaluate_rule(sub, scope.clone())? {
                    return Ok(Some(endpoint));
                }
            }
            Ok(None)
        }
    }
}

/// `EndpointRule.resolve_properties`.
fn resolve_properties(properties: &Json, scope: &Scope) -> Result<Json, EndpointError> {
    Ok(match properties {
        Json::Array(a) => Json::Array(
            a.iter()
                .map(|p| resolve_properties(p, scope))
                .collect::<Result<_, _>>()?,
        ),
        Json::Object(o) => Json::Object(
            o.iter()
                .map(|(k, v)| Ok((k.clone(), resolve_properties(v, scope)?)))
                .collect::<Result<_, EndpointError>>()?,
        ),
        Json::String(s) if stdlib::is_template(s) => {
            Json::String(stdlib::resolve_template_string(s, scope)?)
        }
        other => other.clone(),
    })
}

/// botocore `xform_name` (without the pre-populated cache, which only
/// holds operation names).
pub(crate) fn xform_name(name: &str, sep: &str) -> String {
    if name.contains(sep) {
        return name.to_string();
    }
    static SPECIAL: OnceLock<Regex> = OnceLock::new();
    static FIRST_CAP: OnceLock<Regex> = OnceLock::new();
    static END_CAP: OnceLock<Regex> = OnceLock::new();
    let special = SPECIAL.get_or_init(|| Regex::new(r"[A-Z]{2,}s\n?\z").unwrap());
    let first_cap = FIRST_CAP.get_or_init(|| Regex::new(r"(.)([A-Z][a-z]+)").unwrap());
    let end_cap = END_CAP.get_or_init(|| Regex::new(r"([a-z0-9])([A-Z])").unwrap());
    let mut name = name.to_string();
    if let Some(m) = special.find(&name) {
        let matched = m.as_str().to_string();
        name = format!(
            "{}{sep}{}",
            &name[..name.len() - matched.len()],
            matched.to_lowercase()
        );
    }
    let rep = format!("${{1}}{sep}${{2}}");
    let s1 = first_cap.replace_all(&name, rep.as_str());
    end_cap.replace_all(&s1, rep.as_str()).to_lowercase()
}

/// The `s3` section of the configuration as botocore's
/// `ClientArgsCreator.compute_s3_config` returns it (booleans already
/// normalized by the config provider). `None` means the key is absent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct S3Config {
    pub addressing_style: Option<String>,
    pub use_accelerate_endpoint: Option<bool>,
    pub use_dualstack_endpoint: Option<bool>,
    pub payload_signing_enabled: Option<bool>,
    pub use_arn_region: Option<bool>,
    pub s3_disable_multiregion_access_points: Option<bool>,
}

impl S3Config {
    fn entries(&self) -> Vec<(&'static str, ParamValue)> {
        let mut out = Vec::new();
        if let Some(v) = &self.addressing_style {
            out.push(("addressing_style", ParamValue::String(v.clone())));
        }
        let bools = [
            ("use_accelerate_endpoint", self.use_accelerate_endpoint),
            ("use_dualstack_endpoint", self.use_dualstack_endpoint),
            ("payload_signing_enabled", self.payload_signing_enabled),
            ("use_arn_region", self.use_arn_region),
            (
                "s3_disable_multiregion_access_points",
                self.s3_disable_multiregion_access_points,
            ),
        ];
        for (k, v) in bools {
            if let Some(v) = v {
                out.push((k, ParamValue::Bool(v)));
            }
        }
        out
    }
}

/// Inputs of `ClientArgsCreator.compute_endpoint_resolver_builtin_defaults`.
#[derive(Debug, Clone, Default)]
pub struct BuiltinInputs {
    /// The region passed to the resolver: the endpoint region for `s3` /
    /// `sts` (endpoint prefix) or when no region is configured, else the
    /// configured region (`_build_endpoint_resolver`).
    pub region_name: Option<String>,
    /// `service_model.endpoint_prefix`.
    pub endpoint_prefix: String,
    pub s3_config: S3Config,
    /// `endpoint_bridge._resolve_endpoint_variant_config_var('use_fips_endpoint')`.
    pub use_fips_endpoint: Option<bool>,
    /// `endpoint_bridge._resolve_use_dualstack_endpoint(endpoint_prefix)`.
    pub use_dualstack_endpoint: Option<bool>,
    /// `--endpoint-url` / configured endpoint URL.
    pub endpoint_url: Option<String>,
    /// The credentials' `account_id`, if any.
    pub account_id: Option<String>,
    pub account_id_endpoint_mode: Option<String>,
    pub s3_disable_express_session_auth: Option<bool>,
    /// `client_config.client_context_params`, keyed by the `xform_name`
    /// of the parameter.
    pub client_context_params: IndexMap<String, ParamValue>,
}

/// The builtin values (`EndpointResolverBuiltins`) and the client context
/// (keyed by `xform_name` of the `clientContextParams` name) handed to
/// `EndpointRulesetResolver`. `None` is an absent / `None` builtin.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Builtins {
    /// `AWS::Region`
    pub region: Option<String>,
    /// `AWS::UseFIPS`
    pub use_fips: Option<bool>,
    /// `AWS::UseDualStack`
    pub use_dualstack: Option<bool>,
    /// `AWS::STS::UseGlobalEndpoint`
    pub sts_use_global_endpoint: Option<bool>,
    /// `AWS::S3::UseGlobalEndpoint`
    pub s3_use_global_endpoint: Option<bool>,
    /// `AWS::S3::Accelerate`
    pub s3_accelerate: Option<bool>,
    /// `AWS::S3::ForcePathStyle`
    pub s3_force_path_style: Option<bool>,
    /// `AWS::S3::UseArnRegion`
    pub s3_use_arn_region: Option<bool>,
    /// `AWS::S3::DisableS3ExpressSessionAuth`
    pub s3_disable_express_session_auth: Option<bool>,
    /// `AWS::S3Control::UseArnRegion`
    pub s3control_use_arn_region: Option<bool>,
    /// `AWS::S3::DisableMultiRegionAccessPoints`
    pub s3_disable_multi_region_access_points: Option<bool>,
    /// `SDK::Endpoint`
    pub sdk_endpoint: Option<String>,
    /// `AWS::Auth::AccountId`
    pub account_id: Option<String>,
    /// `AWS::Auth::AccountIdEndpointMode`
    pub account_id_endpoint_mode: Option<String>,
    /// The client context (`client_config.client_context_params`, plus the
    /// `s3` config for S3 / S3 Control).
    pub client_context: IndexMap<String, ParamValue>,
}

/// Every `EndpointResolverBuiltins` value.
pub const BUILTIN_NAMES: [&str; 14] = [
    "AWS::Region",
    "AWS::UseFIPS",
    "AWS::UseDualStack",
    "AWS::STS::UseGlobalEndpoint",
    "AWS::S3::UseGlobalEndpoint",
    "AWS::S3::Accelerate",
    "AWS::S3::ForcePathStyle",
    "AWS::S3::UseArnRegion",
    "AWS::S3::DisableS3ExpressSessionAuth",
    "AWS::S3Control::UseArnRegion",
    "AWS::S3::DisableMultiRegionAccessPoints",
    "SDK::Endpoint",
    "AWS::Auth::AccountId",
    "AWS::Auth::AccountIdEndpointMode",
];

impl Builtins {
    /// The value of a builtin (`EndpointRulesetResolver._resolve_param_as_builtin`).
    pub fn get(&self, name: &str) -> Result<Option<ParamValue>, EndpointError> {
        let s = |v: &Option<String>| v.clone().map(ParamValue::String);
        let b = |v: &Option<bool>| v.map(ParamValue::Bool);
        Ok(match name {
            "AWS::Region" => s(&self.region),
            "AWS::UseFIPS" => b(&self.use_fips),
            "AWS::UseDualStack" => b(&self.use_dualstack),
            "AWS::STS::UseGlobalEndpoint" => b(&self.sts_use_global_endpoint),
            "AWS::S3::UseGlobalEndpoint" => b(&self.s3_use_global_endpoint),
            "AWS::S3::Accelerate" => b(&self.s3_accelerate),
            "AWS::S3::ForcePathStyle" => b(&self.s3_force_path_style),
            "AWS::S3::UseArnRegion" => b(&self.s3_use_arn_region),
            "AWS::S3::DisableS3ExpressSessionAuth" => b(&self.s3_disable_express_session_auth),
            "AWS::S3Control::UseArnRegion" => b(&self.s3control_use_arn_region),
            "AWS::S3::DisableMultiRegionAccessPoints" => {
                b(&self.s3_disable_multi_region_access_points)
            }
            "SDK::Endpoint" => s(&self.sdk_endpoint),
            "AWS::Auth::AccountId" => s(&self.account_id),
            "AWS::Auth::AccountIdEndpointMode" => s(&self.account_id_endpoint_mode),
            other => return Err(EndpointError::UnknownBuiltIn(other.to_string())),
        })
    }

    /// Set a builtin by name; a value of the wrong type or an unknown name
    /// is an error.
    pub fn set(&mut self, name: &str, value: Option<ParamValue>) -> Result<(), EndpointError> {
        fn str_of(v: Option<ParamValue>) -> Option<String> {
            match v {
                Some(ParamValue::String(s)) => Some(s),
                Some(other) => Some(other.py_str()),
                None => None,
            }
        }
        fn bool_of(v: Option<ParamValue>) -> Option<bool> {
            match v {
                Some(ParamValue::Bool(b)) => Some(b),
                Some(_) => Some(true),
                None => None,
            }
        }
        match name {
            "AWS::Region" => self.region = str_of(value),
            "AWS::UseFIPS" => self.use_fips = bool_of(value),
            "AWS::UseDualStack" => self.use_dualstack = bool_of(value),
            "AWS::STS::UseGlobalEndpoint" => self.sts_use_global_endpoint = bool_of(value),
            "AWS::S3::UseGlobalEndpoint" => self.s3_use_global_endpoint = bool_of(value),
            "AWS::S3::Accelerate" => self.s3_accelerate = bool_of(value),
            "AWS::S3::ForcePathStyle" => self.s3_force_path_style = bool_of(value),
            "AWS::S3::UseArnRegion" => self.s3_use_arn_region = bool_of(value),
            "AWS::S3::DisableS3ExpressSessionAuth" => {
                self.s3_disable_express_session_auth = bool_of(value)
            }
            "AWS::S3Control::UseArnRegion" => self.s3control_use_arn_region = bool_of(value),
            "AWS::S3::DisableMultiRegionAccessPoints" => {
                self.s3_disable_multi_region_access_points = bool_of(value)
            }
            "SDK::Endpoint" => self.sdk_endpoint = str_of(value),
            "AWS::Auth::AccountId" => self.account_id = str_of(value),
            "AWS::Auth::AccountIdEndpointMode" => self.account_id_endpoint_mode = str_of(value),
            other => return Err(EndpointError::UnknownBuiltIn(other.to_string())),
        }
        Ok(())
    }

    /// `ClientArgsCreator.compute_endpoint_resolver_builtin_defaults` plus
    /// the client context assembled in `_build_endpoint_resolver`.
    pub fn compute(input: &BuiltinInputs) -> Builtins {
        let given_endpoint = input.endpoint_url.clone().filter(|u| !u.is_empty());
        let s3 = &input.s3_config;
        let addressing = s3.addressing_style.as_deref();
        let force_path_style = if s3.use_accelerate_endpoint.unwrap_or(false) {
            false
        } else if input
            .endpoint_url
            .as_deref()
            .is_some_and(|u| !is_s3_accelerate_url(u))
        {
            addressing != Some("virtual")
        } else {
            addressing == Some("path")
        };
        let mut client_context = input.client_context_params.clone();
        if matches!(input.endpoint_prefix.as_str(), "s3" | "s3-control") {
            for (k, v) in s3.entries() {
                client_context.insert(k.to_string(), v);
            }
            if let Some(v) = input.s3_disable_express_session_auth {
                client_context.insert(
                    "disable_s3_express_session_auth".into(),
                    ParamValue::Bool(v),
                );
            }
        }
        Builtins {
            region: input.region_name.clone(),
            use_fips: Some(given_endpoint.is_none() && input.use_fips_endpoint.unwrap_or(false)),
            use_dualstack: Some(
                given_endpoint.is_none() && input.use_dualstack_endpoint.unwrap_or(false),
            ),
            sts_use_global_endpoint: Some(false),
            s3_use_global_endpoint: Some(false),
            s3_accelerate: Some(s3.use_accelerate_endpoint.unwrap_or(false)),
            s3_force_path_style: Some(force_path_style),
            s3_use_arn_region: Some(s3.use_arn_region.unwrap_or(true)),
            s3_disable_express_session_auth: input.s3_disable_express_session_auth,
            s3control_use_arn_region: Some(s3.use_arn_region.unwrap_or(false)),
            s3_disable_multi_region_access_points: Some(
                s3.s3_disable_multiregion_access_points.unwrap_or(false),
            ),
            sdk_endpoint: given_endpoint,
            account_id: input.account_id.clone(),
            account_id_endpoint_mode: input.account_id_endpoint_mode.clone(),
            client_context,
        }
    }
}

/// botocore `utils.is_s3_accelerate_url`.
fn is_s3_accelerate_url(url: &str) -> bool {
    let Ok((scheme, netloc)) = pyurl::urlparse(url).map(|p| (p.scheme, p.netloc)) else {
        return false;
    };
    if !netloc.ends_with("amazonaws.com") || !(scheme == "https" || scheme == "http") {
        return false;
    }
    let parts: Vec<&str> = netloc.split('.').collect();
    if parts[0] != "s3-accelerate" {
        return false;
    }
    let feature_parts = if parts.len() >= 3 {
        &parts[1..parts.len() - 2]
    } else {
        &[][..]
    };
    let mut seen = std::collections::HashSet::new();
    if !feature_parts.iter().all(|p| seen.insert(*p)) {
        return false;
    }
    feature_parts.iter().all(|p| *p == "dualstack")
}

/// botocore `handlers.customize_endpoint_resolver_builtins`, registered on
/// `before-endpoint-resolution.s3`.
pub fn customize_s3_builtins(builtins: &mut Builtins, operation_name: &str, params: &Value) {
    let bucket_is_arn = params
        .get("Bucket")
        .and_then(Value::as_str)
        .is_some_and(|b| b.starts_with("arn:") && b.splitn(6, ':').count() >= 6);
    if operation_name == "GetBucketLocation" {
        builtins.s3_force_path_style = Some(true);
    } else if bucket_is_arn {
        builtins.s3_force_path_style = Some(false);
    }
}

/// botocore `ServiceId.hyphenize`.
fn hyphenize_service_id(model: &ServiceModel) -> String {
    model
        .metadata_str("serviceId")
        .unwrap_or_default()
        .replace(' ', "-")
        .to_lowercase()
}

/// `S3_UNREFERENCED_PARAMS`, dropped from the provider input for `s3`.
const S3_UNREFERENCED_PARAMS: [&str; 3] = ["Key", "Prefix", "CopySource"];

/// The provider parameters for one call
/// (`EndpointRulesetResolver._get_provider_params`): for each rule set
/// parameter in order, the operation's `staticContextParams`, the input
/// members' `contextParam`, `operationContextParams` (JMESPath over the
/// call parameters), the client context, then the builtin. The
/// `before-endpoint-resolution.s3` customization is applied to a copy of
/// `builtins`, and `Key` / `Prefix` / `CopySource` are dropped for `s3`
/// (`EndpointProvider.resolve_endpoint`).
pub fn build_params(
    model: &ServiceModel,
    operation: &Operation,
    call_params: &Value,
    builtins: &Builtins,
) -> IndexMap<String, ParamValue> {
    let mut customized = builtins.clone();
    if hyphenize_service_id(model) == "s3" {
        customize_s3_builtins(&mut customized, operation.name, call_params);
    }
    provider_params(model, operation, call_params, &customized)
}

fn provider_params(
    model: &ServiceModel,
    operation: &Operation,
    call_params: &Value,
    builtins: &Builtins,
) -> IndexMap<String, ParamValue> {
    let mut out = IndexMap::new();
    let Some(ruleset) = model.ruleset() else {
        return out;
    };
    let Some(specs) = ruleset.get("parameters").and_then(Json::as_object) else {
        return out;
    };
    let static_params = operation
        .get("staticContextParams")
        .and_then(Json::as_object);
    let dynamic_params: Vec<(String, String)> = operation
        .input()
        .map(|input| {
            input
                .members()
                .into_iter()
                .filter_map(|(member, shape)| {
                    let name = shape.member_get("contextParam")?.get("name")?.as_str()?;
                    Some((name.to_string(), member.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let op_context = operation
        .get("operationContextParams")
        .and_then(Json::as_object);
    let client_context_names = model.client_context_params();
    for (name, spec) in specs {
        let mut value = static_params
            .and_then(|s| s.get(name))
            .and_then(|p| p.get("value"))
            .and_then(ParamValue::from_json);
        if value.is_none()
            && let Some((_, member)) = dynamic_params.iter().find(|(n, _)| n == name)
        {
            value = call_params.get(member).and_then(ParamValue::from_value);
        }
        if value.is_none()
            && let Some(path) = op_context
                .and_then(|o| o.get(name))
                .and_then(|p| p.get("path"))
                .and_then(Json::as_str)
        {
            let args = if call_params.as_map().is_some() {
                call_params.clone()
            } else {
                Value::map()
            };
            value = crate::jmes::search(path, &args)
                .ok()
                .as_ref()
                .and_then(ParamValue::from_value);
        }
        if value.is_none() && client_context_names.is_some_and(|c| c.contains_key(name)) {
            value = builtins.client_context.get(&xform_name(name, "_")).cloned();
        }
        if value.is_none()
            && let Some(builtin) = spec.get("builtIn").and_then(Json::as_str)
        {
            value = builtins.get(builtin).ok().flatten();
        }
        if let Some(v) = value {
            out.insert(name.clone(), v);
        }
    }
    if model.name == "s3" {
        for p in S3_UNREFERENCED_PARAMS {
            out.shift_remove(p);
        }
    }
    out
}

/// `EndpointRulesetResolver.construct_endpoint`: build the parameters,
/// resolve, translate rule set errors to botocore exceptions
/// (`ruleset_error_to_botocore_exception`), switch to `http://` when SSL
/// is disabled and no `Endpoint` parameter is set, and keep only the
/// first value of each header.
pub fn construct_endpoint(
    model: &ServiceModel,
    operation: &Operation,
    call_params: &Value,
    builtins: &Builtins,
    use_ssl: bool,
) -> Result<Endpoint, EndpointError> {
    let ruleset = model.ruleset().ok_or_else(|| {
        EndpointError::Internal(format!("no endpoint ruleset for {}", model.name))
    })?;
    let params = build_params(model, operation, call_params, builtins);
    let mut endpoint = match resolve_endpoint(ruleset, &params) {
        Ok(e) => e,
        Err(e) => return Err(ruleset_error_to_botocore_exception(e, &model.name, &params)),
    };
    if !use_ssl && endpoint.url.starts_with("https://") && !params.contains_key("Endpoint") {
        endpoint.url = format!("http://{}", &endpoint.url[8..]);
    }
    for values in endpoint.headers.values_mut() {
        values.truncate(1);
    }
    Ok(endpoint)
}

/// `EndpointRulesetResolver.ruleset_error_to_botocore_exception`.
pub fn ruleset_error_to_botocore_exception(
    error: EndpointError,
    service_name: &str,
    params: &IndexMap<String, ParamValue>,
) -> EndpointError {
    let EndpointError::Resolution(msg) = &error else {
        return error;
    };
    let msg = msg.clone();
    if msg.starts_with("Invalid region in ARN: ") {
        let label = msg.split('`').nth(1).unwrap_or(&msg).to_string();
        return EndpointError::InvalidHostLabel(label);
    }
    if service_name == "s3" {
        if msg == "S3 Object Lambda does not support S3 Accelerate"
            || msg == "Accelerate cannot be used with FIPS"
        {
            return EndpointError::UnsupportedS3Configuration(msg);
        }
        if [
            "S3 Outposts does not support",
            "S3 MRAP does not support",
            "S3 Object Lambda does not support",
            "Access Points do not support",
            "Invalid configuration:",
            "Client was configured for partition",
        ]
        .iter()
        .any(|p| msg.starts_with(p))
        {
            return EndpointError::UnsupportedS3AccesspointConfiguration(msg);
        }
        if msg.to_lowercase().starts_with("invalid arn:") {
            return EndpointError::ParamValidation(msg);
        }
    }
    if service_name == "s3control" {
        if msg.starts_with("Invalid ARN:") {
            let arn = params.get("Bucket").map(ParamValue::py_str);
            return EndpointError::UnsupportedS3ControlArn { arn, msg };
        }
        if msg.starts_with("Invalid configuration:")
            || msg.starts_with("Client was configured for partition")
        {
            return EndpointError::UnsupportedS3ControlConfiguration(msg);
        }
        if msg == "AccountId is required but not set" {
            return EndpointError::ParamValidation(msg);
        }
    }
    if service_name == "events"
        && (msg.starts_with(
            "Invalid Configuration: FIPS is not supported with EventBridge multi-region endpoints.",
        ) || msg == "EndpointId must be a valid host label.")
    {
        return EndpointError::InvalidEndpointConfiguration(msg);
    }
    error
}

/// The auth type and signing context botocore derives from an endpoint's
/// `authSchemes` (`request_context['auth_type']` and
/// `request_context['signing']`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SigningContext {
    /// The botocore auth type (`v4`, `v4a`, `s3v4`, `v4-s3express`,
    /// `bearer`, `none`, ...). `None` when a requested signature version
    /// matched no scheme (botocore falls back to its own logic).
    pub scheme: Option<String>,
    /// `signing['signing_name']`.
    pub signing_name: Option<String>,
    /// `signing['region']`: `signingRegion`, or `signingRegionSet` joined
    /// with `,`.
    pub signing_region: Option<String>,
    /// The raw `signingRegionSet`, when that is where the region came from.
    pub signing_region_set: Option<Vec<String>>,
    /// `signing['disableDoubleEncoding']`.
    pub disable_double_encoding: Option<bool>,
}

/// The client settings that influence auth scheme selection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuthOptions {
    /// `botocore.UNSIGNED` (`--no-sign-request`).
    pub unsigned: bool,
    /// `client_config.signature_version`.
    pub signature_version: Option<String>,
    /// The `auth_scheme_preference` setting (comma separated).
    pub auth_scheme_preference: Option<String>,
}

/// botocore `auth.AUTH_TYPE_MAPS` keys.
const AUTH_TYPE_MAPS: [&str; 15] = [
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

fn auth_type_known(name: &str) -> bool {
    AUTH_TYPE_MAPS.contains(&name)
}

/// botocore `auth.AUTH_PREF_TO_SIGNATURE_VERSION`.
fn auth_pref_to_signature_version(pref: &str) -> Option<&'static str> {
    match pref {
        "sigv4" => Some("v4"),
        "sigv4a" => Some("v4a"),
        "httpBearerAuth" => Some("bearer"),
        "noAuth" => Some("none"),
        _ => None,
    }
}

fn strip_sig_prefix(name: &str) -> &str {
    name.strip_prefix("sig").unwrap_or(name)
}

/// `_does_botocore_authname_match_ruleset_authname`.
fn botocore_authname_matches(botoname: &str, rsname: &str) -> bool {
    let rsname = strip_sig_prefix(rsname);
    let mut botoname = botoname.split('-').next().unwrap_or_default();
    if botoname != "s3" && botoname.starts_with("s3") {
        botoname = &botoname[2..];
    }
    rsname == botoname
}

/// botocore `auth.resolve_auth_scheme_preference`.
fn resolve_auth_scheme_preference(
    preference_list: &[&str],
    auth_options: &[String],
) -> Result<&'static str, EndpointError> {
    let service_supported: Vec<&str> = auth_options
        .iter()
        .map(|s| s.rsplit('#').next().unwrap_or_default())
        .collect();
    let mut seen = Vec::new();
    for s in preference_list.iter().chain(service_supported.iter()) {
        if !seen.contains(s) {
            seen.push(*s);
        }
    }
    for scheme in seen.into_iter().filter(|s| service_supported.contains(s)) {
        if scheme == "noAuth" {
            return Ok("none");
        }
        if let Some(v) = auth_pref_to_signature_version(scheme)
            && auth_type_known(v)
        {
            return Ok(v);
        }
    }
    let mut sorted = service_supported.clone();
    sorted.sort();
    Err(EndpointError::UnsupportedSignatureVersion(
        sorted.join(", "),
    ))
}

/// The signing context for an endpoint with the default client settings.
/// `Ok(None)` when the endpoint has no `authSchemes` property.
pub fn signing_context(endpoint: &Endpoint) -> Result<Option<SigningContext>, EndpointError> {
    signing_context_with(endpoint, &AuthOptions::default())
}

/// `EndpointRulesetResolver.auth_schemes_to_signing_ctx`, applied the way
/// `BaseClient._resolve_endpoint_ruleset` does (only when `authSchemes`
/// is present).
pub fn signing_context_with(
    endpoint: &Endpoint,
    options: &AuthOptions,
) -> Result<Option<SigningContext>, EndpointError> {
    let Some(auth_schemes) = endpoint.properties.get("authSchemes") else {
        return Ok(None);
    };
    let schemes = match auth_schemes.as_array() {
        Some(a) if !a.is_empty() => a,
        _ => {
            return Err(EndpointError::Internal(
                "auth_schemes must be a non-empty list.".into(),
            ));
        }
    };
    if options.unsigned {
        return Ok(Some(SigningContext {
            scheme: Some("none".into()),
            ..Default::default()
        }));
    }
    let scheme_name = |s: &Json| -> Result<String, EndpointError> {
        s.get("name")
            .and_then(Json::as_str)
            .map(str::to_string)
            .ok_or_else(|| EndpointError::Internal("'name'".into()))
    };
    let names: Vec<String> = schemes.iter().map(scheme_name).collect::<Result<_, _>>()?;
    let stripped: Vec<&str> = names.iter().map(|n| strip_sig_prefix(n)).collect();
    let (name, scheme): (String, &Json) = if let Some(requested) = &options.signature_version {
        match stripped
            .iter()
            .position(|s| botocore_authname_matches(requested, s))
        {
            Some(i) => (requested.clone(), &schemes[i]),
            None => {
                return Ok(Some(SigningContext::default()));
            }
        }
    } else if let Some(pref) = &options.auth_scheme_preference {
        let prefs: Vec<&str> = pref.split(',').collect();
        let available: Vec<String> = names
            .iter()
            .map(|n| n.rsplit('#').next().unwrap_or_default().to_string())
            .collect();
        let name = resolve_auth_scheme_preference(&prefs, &available)?;
        let mut by_type: IndexMap<&str, &Json> = IndexMap::new();
        for (n, s) in names.iter().zip(schemes) {
            by_type.insert(
                strip_sig_prefix(n.rsplit('#').next().unwrap_or_default()),
                s,
            );
        }
        let scheme = by_type
            .get(name)
            .copied()
            .ok_or_else(|| EndpointError::Internal(crate::pyfmt::str_repr(name)))?;
        (name.to_string(), scheme)
    } else {
        match stripped.iter().position(|s| auth_type_known(s)) {
            Some(i) => (stripped[i].to_string(), &schemes[i]),
            None => {
                return Err(EndpointError::UnknownSignatureVersion(stripped.join(", ")));
            }
        }
    };
    let mut ctx = SigningContext {
        scheme: Some(name),
        ..Default::default()
    };
    if let Some(region) = scheme.get("signingRegion") {
        ctx.signing_region = Some(json_py_str(region));
    } else if let Some(set) = scheme.get("signingRegionSet") {
        let set: Vec<String> = set
            .as_array()
            .map(|a| a.iter().map(json_py_str).collect())
            .unwrap_or_default();
        if !set.is_empty() {
            ctx.signing_region = Some(set.join(","));
        }
        ctx.signing_region_set = Some(set);
    }
    if let Some(n) = scheme.get("signingName") {
        ctx.signing_name = Some(json_py_str(n));
    }
    if let Some(d) = scheme.get("disableDoubleEncoding") {
        ctx.disable_double_encoding = Some(match d {
            Json::Bool(b) => *b,
            Json::String(s) => s.to_lowercase() == "true",
            _ => false,
        });
    }
    Ok(Some(ctx))
}

fn json_py_str(v: &Json) -> String {
    Val::from_json(v).py_str()
}

#[cfg(test)]
mod tests;
