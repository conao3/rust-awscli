//! botocore handlers that run around every call (`botocore/handlers.py`
//! `BUILTIN_HANDLERS`, `botocore/client.py` `_make_api_call`,
//! `botocore/httpchecksum.py`, `botocore/compress.py`) and the CLI's
//! handlers that change requests and responses of model-driven
//! operations.
//!
//! Each event runs the handlers in botocore's dispatch order: the most
//! specific event name first (`<event>.<service-id>.<Operation>`, then
//! `<event>.<service-id>`, then `<event>`), and in registration order for
//! one name.

use std::io::Write;

use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha1::Digest as _;

use crate::config::ConfigLookup;
use crate::model::{Operation, ServiceModel};
use crate::protocol::{HttpRequest, HttpResponse};
use crate::value::{Map, Value};

mod decode;
mod glacier;
pub mod presign;
mod s3;
mod services;

#[cfg(test)]
mod tests;

pub use presign::PresignEnv;

/// The `request_context` entries handlers share between the events of
/// one call.
#[derive(Debug, Clone, Default)]
pub struct CallContext {
    /// `context['encoding_type_auto_set']`.
    pub encoding_type_auto_set: bool,
}

/// botocore `ParamValidationError` raised by a handler.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("Parameter validation failed:\n{0}")]
pub struct ParamValidationError(pub String);

/// A Python exception a handler lets propagate; `Display` is
/// `str(exception)`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct PythonError(pub String);

/// `KeyError(key)`.
fn key_error(key: &str) -> anyhow::Error {
    PythonError(format!("'{key}'")).into()
}

/// `ServiceId.hyphenize()`, the service part of event names.
pub fn service_id(model: &ServiceModel) -> String {
    crate::session::hyphenize(model.metadata_str("serviceId").unwrap_or(&model.name))
}

/// `urllib.parse.unquote`: `%XX` escapes decoded as UTF-8 with
/// replacement; other characters kept.
pub fn unquote(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut run: Vec<u8> = Vec::new();
    let bytes = s.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16);
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !b.is_ascii() {
            out.push_str(&String::from_utf8_lossy(&run));
            run.clear();
            let c = s[i..].chars().next().expect("char boundary");
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        if b == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            run.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        run.push(b);
        i += 1;
    }
    out.push_str(&String::from_utf8_lossy(&run));
    out
}

/// `before-parameter-build.<service-id>.<Operation>`: the service handlers
/// (most specific first), then `generate_idempotent_uuid` and
/// `_handle_request_validation_mode_member`.
pub fn before_parameter_build(
    model: &ServiceModel,
    operation: &Operation<'_>,
    params: &mut Value,
    config: &dyn ConfigLookup,
) -> Result<CallContext> {
    let mut context = CallContext::default();
    let Some(map) = params.as_map_mut() else {
        return Ok(context);
    };
    alias_parameters(model, operation, map)?;
    match service_id(model).as_str() {
        "s3" => s3::before_parameter_build(operation, map, &mut context)?,
        "ec2" if operation.name == "RunInstances" => services::base64_encode_user_data(map),
        "auto-scaling" if operation.name == "CreateLaunchConfiguration" => {
            services::base64_encode_user_data(map)
        }
        "route-53" => services::fix_route53_ids(operation, map),
        "glacier" => glacier::inject_account_id(map),
        _ => {}
    }
    if let Some(input) = operation.input() {
        for (name, member) in input.members() {
            if member.get_bool("idempotencyToken") && !map.contains_key(name) {
                map.insert(
                    name.to_string(),
                    Value::str(uuid::Uuid::new_v4().to_string()),
                );
            }
        }
    }
    if let Some(mode_member) = http_checksum(operation)
        .and_then(|c| c.get("requestValidationModeMember"))
        .and_then(|v| v.as_str())
        && config
            .config_variable("response_checksum_validation")
            .as_deref()
            .unwrap_or("when_supported")
            == "when_supported"
        && !map.contains_key(mode_member)
    {
        map.insert(mode_member.to_string(), Value::str("ENABLED"));
    }
    Ok(context)
}

/// `_add_parameter_aliases`: `(service-id, operation or *, original,
/// alias)`.
const PARAMETER_ALIASES: &[(&str, &str, &str, &str)] = &[
    ("ec2", "*", "Filter", "Filters"),
    ("cloudwatch-logs", "CreateExportTask", "from", "fromTime"),
    ("cloudsearch-domain", "Search", "return", "returnFields"),
];

/// `ParameterAlias.alias_parameter_in_call` (registered first): a value
/// given under the alias moves to the modeled name.
pub fn alias_parameters(
    model: &ServiceModel,
    operation: &Operation<'_>,
    params: &mut Map,
) -> Result<()> {
    let service = service_id(model);
    for &(svc, op, original, alias) in PARAMETER_ALIASES {
        if svc != service || (op != "*" && op != operation.name) {
            continue;
        }
        let modeled = operation
            .input()
            .is_some_and(|i| i.member_shape(original).is_some());
        if !modeled || !params.contains_key(alias) {
            continue;
        }
        if params.contains_key(original) {
            return Err(PythonError(format!(
                "Parameter '{original}' and its alias '{alias}' were provided for operation {}.  Only one of them may be used.",
                operation.name
            ))
            .into());
        }
        let value = params.shift_remove(alias).expect("checked");
        params.insert(original.to_string(), value);
    }
    Ok(())
}

/// `before-call.<service-id>.<Operation>`: the service handlers (most
/// specific first), then `inject_api_version_header_if_needed`.
pub fn before_call(
    model: &ServiceModel,
    operation: &Operation<'_>,
    params: &Value,
    request: &mut HttpRequest,
    presign: &PresignEnv<'_>,
) -> Result<()> {
    match (service_id(model).as_str(), operation.name) {
        ("s3", _) => s3::before_call(operation, params, request),
        ("glacier", _) => glacier::before_call(model, operation, request),
        ("api-gateway", _) => add_accept_header(request),
        ("ec2", "CopySnapshot") => presign::inject_presigned_url_ec2(request, presign)?,
        (
            "rds",
            "CopyDBClusterSnapshot"
            | "CreateDBCluster"
            | "CopyDBSnapshot"
            | "CreateDBInstanceReadReplica"
            | "StartDBInstanceAutomatedBackupsReplication",
        )
        | ("neptune" | "docdb", "CopyDBClusterSnapshot" | "CreateDBCluster") => {
            presign::inject_presigned_url_rds(request, presign)?
        }
        _ => {}
    }
    inject_api_version_header(model, operation, request);
    Ok(())
}

/// `request-created.<service-id>.<Operation>` before the request is
/// signed: `switch_host_machinelearning` and `change_get_to_post`. `url` is
/// the request URL; returns the new URL when a handler changed it (the
/// body and method are changed in place).
pub fn request_created(
    model: &ServiceModel,
    operation: &Operation<'_>,
    request: &mut HttpRequest,
    url: &str,
) -> Option<String> {
    match (service_id(model).as_str(), operation.name) {
        ("machine-learning", "Predict") => {
            services::switch_host_machinelearning(url, &request.body)
        }
        ("cloudsearch-domain", "Search") => {
            let (url, body) = services::change_get_to_post(&request.method, url)?;
            set_header(
                &mut request.headers,
                "Content-Type",
                "application/x-www-form-urlencoded",
            );
            request.method = "POST".to_string();
            request.body = body;
            Some(url)
        }
        _ => None,
    }
}

/// `before-parse.<service-id>.<Operation>`: may change the response
/// before it is parsed; returns `customized_response_dict`, merged into
/// the parsed response.
pub fn before_parse(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &mut HttpResponse,
) -> Map {
    if service_id(model) == "s3" {
        s3::before_parse(operation, response)
    } else {
        Map::new()
    }
}

/// The `before-parse` changes that apply to every attempt before retries
/// are decided (`_handle_200_error` with `_update_status_code`).
pub fn adjust_attempt_response(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &mut HttpResponse,
) {
    if service_id(model) == "s3" {
        s3::handle_200_error(operation, response);
    }
}

/// `after-call.<service-id>.<Operation>` on the parsed response (success
/// or error). `paginate` is the CLI's `parsed_globals.paginate`.
pub fn after_call(
    model: &ServiceModel,
    operation: &Operation<'_>,
    parsed: &mut Value,
    response: &HttpResponse,
    context: &CallContext,
    paginate: bool,
) -> Result<()> {
    match (service_id(model).as_str(), operation.name) {
        ("iam", _) => decode::json_decode_policies(parsed, operation.output()),
        ("ec2", "GetConsoleOutput") => decode::decode_console_output(parsed),
        ("cloudformation", "GetTemplate") => decode::json_decode_template_body(parsed),
        ("s3", _) => s3::after_call(operation, parsed, response, context)?,
        ("data-pipeline", "GetPipelineDefinition") => decode::api_to_definition(parsed)?,
        ("dynamodb", _) if paginate => decode::parse_last_evaluated_key_binary(parsed)?,
        _ => {}
    }
    Ok(())
}
fn http_checksum<'a>(
    operation: &Operation<'a>,
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    operation.get("httpChecksum")?.as_object()
}

/// The checksum to apply to the request (`resolve_request_checksum_algorithm`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestChecksum {
    pub algorithm: String,
    /// `x-amz-checksum-<algorithm>`.
    pub header: String,
    /// The header targeted by `requestAlgorithmMember`, set to `CRC64NVME` when
    /// the algorithm was chosen by default.
    pub algorithm_header: Option<String>,
}

/// `httpchecksum.DEFAULT_CHECKSUM_ALGORITHM`.
const DEFAULT_CHECKSUM_ALGORITHM: &str = "CRC64NVME";

const SUPPORTED_CHECKSUM_ALGORITHMS: &[&str] = &[
    "crc32",
    "crc32c",
    "crc64nvme",
    "sha1",
    "sha256",
    "sha512",
    "xxhash64",
    "xxhash3",
    "xxhash128",
    "md5",
];

/// `httpchecksum.resolve_request_checksum_algorithm`.
pub fn resolve_request_checksum(
    operation: &Operation<'_>,
    params: &Value,
    request: &HttpRequest,
    config: &dyn ConfigLookup,
) -> Result<Option<RequestChecksum>> {
    if request
        .headers
        .iter()
        .any(|(k, _)| k.to_ascii_lowercase().starts_with("x-amz-checksum-"))
    {
        return Ok(None);
    }
    let checksum = http_checksum(operation);
    let required = operation
        .get("httpChecksumRequired")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || checksum
            .and_then(|c| c.get("requestChecksumRequired"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    let algorithm_member = checksum
        .and_then(|c| c.get("requestAlgorithmMember"))
        .and_then(|v| v.as_str());
    let calculation = config
        .config_variable("request_checksum_calculation")
        .unwrap_or_else(|| "when_supported".to_string());
    let (algorithm, algorithm_header) = match algorithm_member
        .and_then(|m| params.get(m))
        .and_then(Value::as_str)
    {
        Some(name) => {
            let name = name.to_lowercase();
            if !SUPPORTED_CHECKSUM_ALGORITHMS.contains(&name.as_str()) {
                bail!("Unsupported checksum algorithm: {name}");
            }
            (name, None)
        }
        None if required || (algorithm_member.is_some() && calculation == "when_supported") => {
            let header = algorithm_member.and_then(|m| {
                let input = operation.input()?;
                let shape = input.member_shape(m)?;
                shape
                    .member_get("locationName")?
                    .as_str()
                    .map(str::to_string)
            });
            (DEFAULT_CHECKSUM_ALGORITHM.to_lowercase(), header)
        }
        None => return Ok(None),
    };
    Ok(Some(RequestChecksum {
        header: format!("x-amz-checksum-{algorithm}"),
        algorithm,
        algorithm_header,
    }))
}

/// `httpchecksum.apply_request_checksum` (header variant).
pub fn apply_request_checksum(request: &mut HttpRequest, checksum: &RequestChecksum) -> Result<()> {
    if !request
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case(&checksum.header))
    {
        let digest = checksum_digest(&checksum.algorithm, &request.body)?;
        set_header(&mut request.headers, &checksum.header, &digest);
    }
    if let Some(name) = &checksum.algorithm_header {
        set_header(&mut request.headers, name, DEFAULT_CHECKSUM_ALGORITHM);
    }
    Ok(())
}

fn checksum_digest(algorithm: &str, body: &[u8]) -> Result<String> {
    let bytes: Vec<u8> = match algorithm {
        "crc32" => crc32fast::hash(body).to_be_bytes().to_vec(),
        "crc32c" => crc32c::crc32c(body).to_be_bytes().to_vec(),
        "crc64nvme" => {
            let mut d = crc64fast_nvme::Digest::new();
            d.write(body);
            d.sum64().to_be_bytes().to_vec()
        }
        "sha1" => sha1::Sha1::digest(body).to_vec(),
        "sha256" => sha2::Sha256::digest(body).to_vec(),
        "sha512" => sha2::Sha512::digest(body).to_vec(),
        "md5" => md5::Md5::digest(body).to_vec(),
        "xxhash64" => xxhash_rust::xxh64::xxh64(body, 0).to_be_bytes().to_vec(),
        "xxhash3" => xxhash_rust::xxh3::xxh3_64(body).to_be_bytes().to_vec(),
        "xxhash128" => xxhash_rust::xxh3::xxh3_128(body).to_be_bytes().to_vec(),
        other => bail!("Unsupported checksum algorithm: {other}"),
    };
    Ok(STANDARD.encode(bytes))
}

/// `inject_api_version_header_if_needed`.
pub fn inject_api_version_header(
    model: &ServiceModel,
    operation: &Operation<'_>,
    request: &mut HttpRequest,
) {
    if operation
        .get("endpointoperation")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        set_header(
            &mut request.headers,
            "x-amz-api-version",
            &model.api_version,
        );
    }
}

/// `compress.maybe_compress_request`.
pub fn maybe_compress_request(
    operation: &Operation<'_>,
    request: &mut HttpRequest,
    config: &dyn ConfigLookup,
) -> Result<()> {
    let Some(compression) = operation.get("requestcompression") else {
        return Ok(());
    };
    if config
        .config_variable("disable_request_compression")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
    {
        return Ok(());
    }
    let min_size: usize = config
        .config_variable("request_min_compression_size_bytes")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10240);
    if request.body.len() < min_size {
        return Ok(());
    }
    let encodings = compression
        .get("encodings")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for encoding in encodings {
        if encoding.as_str() == Some("gzip") {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(9));
            enc.write_all(&request.body)?;
            request.body = enc.finish()?;
            let header = match request
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("Content-Encoding"))
            {
                Some((_, v)) => format!("{v},gzip"),
                None => "gzip".to_string(),
            };
            set_header(&mut request.headers, "Content-Encoding", &header);
            return Ok(());
        }
    }
    Ok(())
}

/// `headers[name] = value` on botocore's request dict headers.
pub fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if let Some(entry) = headers
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
    {
        entry.1 = value.to_string();
    } else {
        headers.push((name.to_string(), value.to_string()));
    }
}

/// `add_accept_header` (`before-call.api-gateway`).
fn add_accept_header(request: &mut HttpRequest) {
    if !request
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("Accept"))
    {
        request
            .headers
            .push(("Accept".to_string(), "application/json".to_string()));
    }
}
