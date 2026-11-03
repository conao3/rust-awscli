//! botocore handlers that run around every call, keyed by model traits
//! (`botocore/handlers.py` `BUILTIN_HANDLERS`, `botocore/client.py`
//! `_make_api_call`, `botocore/httpchecksum.py`, `botocore/compress.py`).

use std::io::Write;

use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha1::Digest as _;

use crate::config::ConfigLookup;
use crate::model::{Operation, ServiceModel};
use crate::protocol::HttpRequest;
use crate::value::Value;

/// `before-parameter-build` handlers: `generate_idempotent_uuid` and
/// `_handle_request_validation_mode_member`.
pub fn before_parameter_build(
    operation: &Operation<'_>,
    params: &mut Value,
    config: &dyn ConfigLookup,
) {
    let Some(map) = params.as_map_mut() else {
        return;
    };
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
    /// The header targeted by `requestAlgorithmMember`, set to `CRC32` when
    /// the algorithm was chosen by default.
    pub algorithm_header: Option<String>,
}

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
            ("crc32".to_string(), header)
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
        set_header(&mut request.headers, name, "CRC32");
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
pub fn add_accept_header(model: &ServiceModel, request: &mut HttpRequest) {
    if model.metadata_str("serviceId") == Some("API Gateway")
        && !request
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("Accept"))
    {
        request
            .headers
            .push(("Accept".to_string(), "application/json".to_string()));
    }
}
