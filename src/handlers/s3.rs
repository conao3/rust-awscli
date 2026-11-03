//! The s3 handlers of `botocore/handlers.py` and the CLI's
//! `awscli/customizations/s3errormsg.py`.

use std::sync::OnceLock;

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use md5::Digest as _;
use regex::Regex;

use super::{CallContext, ParamValidationError, key_error, unquote};
use crate::model::Operation;
use crate::protocol::timestamp::{self, WireTimestamp};
use crate::protocol::{HttpRequest, HttpResponse, TimestampFormat};
use crate::session::url::quote;
use crate::value::{Map, Value};

/// `VALID_BUCKET.pattern`.
const VALID_BUCKET_PATTERN: &str = r"^[a-zA-Z0-9.\-_]{1,255}$";
/// `VALID_S3_ARN.pattern`.
const VALID_S3_ARN_PATTERN: &str = r"^arn:(aws).*:(s3|s3-object-lambda):[a-z\-0-9]*:[0-9]{12}:accesspoint[/:][a-zA-Z0-9\-.]{1,63}$|^arn:(aws).*:s3-outposts:[a-z\-0-9]+:[0-9]{12}:outpost[/:][a-zA-Z0-9\-]{1,63}[/:]accesspoint[/:][a-zA-Z0-9\-]{1,63}$";

/// A Python `re` pattern ending in `$`, which also matches before a final
/// newline.
fn python_dollar(pattern: &str) -> Regex {
    Regex::new(&pattern.replace('$', r"\n?\z")).expect("valid pattern")
}

fn valid_bucket() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| python_dollar(VALID_BUCKET_PATTERN))
}

fn valid_s3_arn() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| python_dollar(VALID_S3_ARN_PATTERN))
}

/// `VERSION_ID_SUFFIX`.
fn version_id_suffix() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| python_dollar(r"\?versionId=[^\s]+$"))
}

/// The `before-parameter-build.s3.<Operation>` and
/// `before-parameter-build.s3` handlers, most specific first.
pub(super) fn before_parameter_build(
    operation: &Operation<'_>,
    params: &mut Map,
    context: &mut CallContext,
) -> Result<()> {
    match operation.name {
        "ListObjects" | "ListObjectsV2" | "ListObjectVersions" => {
            set_list_objects_encoding_type_url(params, context);
        }
        "CopyObject" => {
            handle_copy_source_param(params)?;
            validate_ascii_metadata(params)?;
            sse_md5(params, "SSECustomer");
            sse_md5(params, "CopySourceSSECustomer");
        }
        "UploadPartCopy" => {
            handle_copy_source_param(params)?;
            sse_md5(params, "SSECustomer");
            sse_md5(params, "CopySourceSSECustomer");
        }
        "PutObject" | "CreateMultipartUpload" => {
            validate_ascii_metadata(params)?;
            sse_md5(params, "SSECustomer");
        }
        "HeadObject" | "GetObject" | "UploadPart" | "CompleteMultipartUpload" => {
            sse_md5(params, "SSECustomer");
        }
        _ => {}
    }
    validate_bucket_name(params)
}

/// `validate_bucket_name`.
fn validate_bucket_name(params: &Map) -> Result<()> {
    let Some(Value::Str(bucket)) = params.get("Bucket") else {
        return Ok(());
    };
    if !valid_bucket().is_match(bucket) && !valid_s3_arn().is_match(bucket) {
        return Err(ParamValidationError(format!(
            "Invalid bucket name \"{bucket}\": Bucket name must match the regex \"{VALID_BUCKET_PATTERN}\" or be an ARN matching the regex \"{VALID_S3_ARN_PATTERN}\""
        ))
        .into());
    }
    Ok(())
}

/// `set_list_objects_encoding_type_url`.
fn set_list_objects_encoding_type_url(params: &mut Map, context: &mut CallContext) {
    if !params.contains_key("EncodingType") {
        context.encoding_type_auto_set = true;
        params.insert("EncodingType".to_string(), Value::str("url"));
    }
}

/// `handle_copy_source_param`.
fn handle_copy_source_param(params: &mut Map) -> Result<()> {
    let quoted = match params.get("CopySource") {
        Some(Value::Str(source)) => quote_source_header(source),
        Some(Value::Map(source)) => quote_source_header_from_dict(source)?,
        _ => return Ok(()),
    };
    params.insert("CopySource".to_string(), Value::Str(quoted));
    Ok(())
}

/// `_quote_source_header_from_dict`.
fn quote_source_header_from_dict(source: &Map) -> Result<String> {
    let missing = |key: &str| ParamValidationError(format!("Missing required parameter: '{key}'"));
    let bucket = source.get("Bucket").ok_or_else(|| missing("Bucket"))?;
    let key = source.get("Key").ok_or_else(|| missing("Key"))?;
    let bucket = crate::pyfmt::py_str(bucket);
    let key = crate::pyfmt::py_str(key);
    let joined = if valid_s3_arn().is_match(&bucket) {
        format!("{bucket}/object/{key}")
    } else {
        format!("{bucket}/{key}")
    };
    let mut quoted = quote(&joined, "-._~/");
    if let Some(version_id) = source.get("VersionId").filter(|v| **v != Value::Null) {
        quoted.push_str(&format!("?versionId={}", crate::pyfmt::py_str(version_id)));
    }
    Ok(quoted)
}

/// `_quote_source_header`.
pub(super) fn quote_source_header(value: &str) -> String {
    match version_id_suffix().find(value) {
        None => quote(value, "-._~/"),
        Some(m) => format!(
            "{}{}",
            quote(&value[..m.start()], "-._~/"),
            &value[m.start()..]
        ),
    }
}

/// `validate_ascii_metadata`.
fn validate_ascii_metadata(params: &Map) -> Result<()> {
    let Some(Value::Map(metadata)) = params.get("Metadata") else {
        return Ok(());
    };
    for (key, value) in metadata {
        let value = match value {
            Value::Str(s) => s.as_str(),
            _ => continue,
        };
        if !key.is_ascii() || !value.is_ascii() {
            return Err(ParamValidationError(format!(
                "Non ascii characters found in S3 metadata for key \"{key}\", value: \"{value}\".  \nS3 metadata can only contain ASCII characters. "
            ))
            .into());
        }
    }
    Ok(())
}

/// `_sse_md5` for the members prefixed by `prefix`.
pub(super) fn sse_md5(params: &mut Map, prefix: &str) {
    let key_member = format!("{prefix}Key");
    let md5_member = format!("{prefix}KeyMD5");
    if params.contains_key(&md5_member) {
        return;
    }
    let key: Vec<u8> = match params.get(&key_member) {
        Some(Value::Str(s)) => s.as_bytes().to_vec(),
        Some(Value::Bytes(b)) => b.clone(),
        _ => return,
    };
    let digest = md5::Md5::digest(&key);
    params.insert(key_member, Value::Str(STANDARD.encode(&key)));
    params.insert(md5_member, Value::Str(STANDARD.encode(digest)));
}

/// `before-call.s3.<Operation>` and `before-call.s3`.
pub(super) fn before_call(operation: &Operation<'_>, params: &Value, request: &mut HttpRequest) {
    if matches!(
        operation.name,
        "DeleteObjects" | "PutBucketLifecycleConfiguration"
    ) {
        escape_xml_payload(request);
    }
    add_expect_header(operation, params, request);
}

/// `escape_xml_payload`.
fn escape_xml_payload(request: &mut HttpRequest) {
    if !request.body.iter().any(|b| matches!(b, b'\r' | b'\n')) {
        return;
    }
    let mut body = Vec::with_capacity(request.body.len());
    for &b in &request.body {
        match b {
            b'\r' => body.extend_from_slice(b"&#xD;"),
            b'\n' => body.extend_from_slice(b"&#xA;"),
            other => body.push(other),
        }
    }
    request.body = body;
}

/// `add_expect_header`: the body is file-like when the operation's
/// payload is a streaming blob the caller provided (the CLI opens the
/// file, and `convert_body_to_file_like_object` wraps `str` and `bytes`).
fn add_expect_header(operation: &Operation<'_>, params: &Value, request: &mut HttpRequest) {
    if !matches!(operation.http_method(), "PUT" | "POST") {
        return;
    }
    let Some(input) = operation.input() else {
        return;
    };
    let Some(payload) = input.get_str("payload") else {
        return;
    };
    let file_like = input
        .member_shape(payload)
        .is_some_and(|s| s.type_name() == "blob" && s.is_streaming())
        && params.get(payload).is_some();
    if file_like {
        super::set_header(&mut request.headers, "Expect", "100-continue");
    }
}

/// `before-parse.s3.*`: `_handle_200_error` (registered first) and
/// `handle_expires_header`. Returns `customized_response_dict`.
pub(super) fn before_parse(operation: &Operation<'_>, response: &mut HttpResponse) -> Map {
    handle_200_error(operation, response);
    let mut customized = Map::new();
    if has_expires_shape(operation)
        && let Some(expires) = header_value(response, "Expires")
        && !expires.is_empty()
    {
        customized.insert("ExpiresString".to_string(), Value::Str(expires.clone()));
        let parsed =
            timestamp::try_output_value(WireTimestamp::Text(expires), TimestampFormat::Iso8601);
        if parsed.is_err() {
            response
                .headers
                .retain(|(k, _)| !k.eq_ignore_ascii_case("Expires"));
        }
    }
    customized
}

/// `_handle_200_error`: an error document with status 200 becomes a 500
/// (for retries too, through `_update_status_code`).
pub(super) fn handle_200_error(operation: &Operation<'_>, response: &mut HttpResponse) {
    if should_handle_200_error(operation) && looks_like_special_case_error(response) {
        response.status = 500;
    }
}

/// The value of a header as urllib3's `HTTPHeaderDict.get` returns it
/// (repeated headers joined with `, `).
fn header_value(response: &HttpResponse, name: &str) -> Option<String> {
    let values: Vec<&str> = response
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// `_has_expires_shape`.
fn has_expires_shape(operation: &Operation<'_>) -> bool {
    operation.output().is_some_and(|output| {
        output.members().iter().any(|(_, member)| {
            member.name == "Expires"
                && member.member_get("locationName").and_then(|v| v.as_str()) == Some("Expires")
        })
    })
}

/// `_should_handle_200_error`.
fn should_handle_200_error(operation: &Operation<'_>) -> bool {
    let Some(output) = operation.output() else {
        return false;
    };
    if crate::protocol::common::has_event_stream_output(operation) {
        return false;
    }
    if let Some(payload) = output.get_str("payload")
        && let Some(shape) = output.member_shape(payload)
        && matches!(shape.type_name(), "blob" | "string")
    {
        return false;
    }
    true
}

/// `_looks_like_special_case_error`.
fn looks_like_special_case_error(response: &HttpResponse) -> bool {
    if response.status != 200 || response.body.is_empty() {
        return false;
    }
    match crate::protocol::xml_common::parse_xml_inner(&response.body) {
        Err(_) => true,
        Ok(root) => root.tag == "Error",
    }
}

/// `after-call.s3.<Operation>` and the CLI's `after-call.s3`
/// (`enhance_error_msg`).
pub(super) fn after_call(
    operation: &Operation<'_>,
    parsed: &mut Value,
    response: &HttpResponse,
    context: &CallContext,
) -> Result<()> {
    match operation.name {
        "GetBucketLocation" => parse_get_bucket_location(parsed, response)?,
        "ListObjects" => decode_list_object(
            parsed,
            context,
            &["Delimiter", "Marker", "NextMarker"],
            &[("Contents", "Key"), ("CommonPrefixes", "Prefix")],
        )?,
        "ListObjectsV2" => decode_list_object(
            parsed,
            context,
            &["Delimiter", "Prefix", "StartAfter"],
            &[("Contents", "Key"), ("CommonPrefixes", "Prefix")],
        )?,
        "ListObjectVersions" => decode_list_object(
            parsed,
            context,
            &["KeyMarker", "NextKeyMarker", "Prefix", "Delimiter"],
            &[
                ("Versions", "Key"),
                ("DeleteMarkers", "Key"),
                ("CommonPrefixes", "Prefix"),
            ],
        )?,
        _ => {}
    }
    enhance_error_msg(parsed)
}

/// `parse_get_bucket_location`: the root element's text.
fn parse_get_bucket_location(parsed: &mut Value, response: &HttpResponse) -> Result<()> {
    let root =
        crate::protocol::xml_common::parse_xml_inner(&response.body).map_err(super::PythonError)?;
    let region = match root.text {
        Some(t) if !t.is_empty() => Value::Str(t),
        _ => Value::Null,
    };
    if let Some(map) = parsed.as_map_mut() {
        map.insert("LocationConstraint".to_string(), region);
    }
    Ok(())
}

/// `_decode_list_object`.
fn decode_list_object(
    parsed: &mut Value,
    context: &CallContext,
    top_level_keys: &[&str],
    nested_keys: &[(&str, &str)],
) -> Result<()> {
    let Some(map) = parsed.as_map_mut() else {
        return Ok(());
    };
    if !(map.get("EncodingType").and_then(Value::as_str) == Some("url")
        && context.encoding_type_auto_set)
    {
        return Ok(());
    }
    for key in top_level_keys {
        if let Some(Value::Str(v)) = map.get_mut(*key) {
            *v = unquote(v);
        }
    }
    for (top_key, child_key) in nested_keys {
        if let Some(Value::List(members)) = map.get_mut(*top_key) {
            for member in members {
                let value = member
                    .as_map_mut()
                    .and_then(|m| m.get_mut(*child_key))
                    .ok_or_else(|| key_error(child_key))?;
                if let Value::Str(v) = value {
                    *v = unquote(v);
                }
            }
        }
    }
    Ok(())
}

const REGION_ERROR_MSG: &str = "You can fix this issue by explicitly providing the correct region location using the --region argument, the AWS_DEFAULT_REGION environment variable, or the region variable in the AWS CLI configuration file.  You can get the bucket's location by running \"aws s3api get-bucket-location --bucket BUCKET\".";

const ENABLE_SIGV4_MSG: &str = " You can enable AWS Signature Version 4 by running the command: \naws configure set s3.signature_version s3v4";

/// `s3errormsg.enhance_error_msg`.
fn enhance_error_msg(parsed: &mut Value) -> Result<()> {
    let Some(error) = parsed
        .as_map_mut()
        .and_then(|m| m.get_mut("Error"))
        .and_then(Value::as_map_mut)
    else {
        return Ok(());
    };
    let message = error
        .get("Message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if message.contains("Please use AWS4-HMAC-SHA256") {
        error.insert(
            "Message".to_string(),
            Value::Str(format!(
                "You are attempting to operate on a bucket in a region that requires Signature Version 4.  {REGION_ERROR_MSG}"
            )),
        );
    } else if error.get("Code").and_then(Value::as_str) == Some("PermanentRedirect") {
        let endpoint =
            crate::pyfmt::py_str(error.get("Endpoint").ok_or_else(|| key_error("Endpoint"))?);
        let message = error.get("Message").ok_or_else(|| key_error("Message"))?;
        let message = crate::pyfmt::py_str(message);
        let mut chars = message.chars();
        chars.next_back();
        error.insert(
            "Message".to_string(),
            Value::Str(format!(
                "{}: {endpoint}\n{REGION_ERROR_MSG}",
                chars.as_str()
            )),
        );
    } else if message.contains("AWS KMS managed keys require AWS Signature Version 4") {
        error.insert(
            "Message".to_string(),
            Value::Str(format!("{message}{ENABLE_SIGV4_MSG}")),
        );
    }
    Ok(())
}
