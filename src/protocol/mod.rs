//! Request serialization and response parsing for the botocore protocols
//! (`botocore/serialize.py`, `botocore/parsers.py`).

use anyhow::{Result, bail};

use crate::model::{Operation, ServiceModel};
use crate::value::Value;

pub mod cbor;
pub mod common;
pub mod ec2;
pub mod json;
pub mod query;
pub mod rest_json;
pub mod rest_xml;
pub mod timestamp;

#[cfg(test)]
mod tests;

/// botocore's serialized `request_dict`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HttpRequest {
    pub method: String,
    /// The percent-encoded path relative to the endpoint, with URI labels
    /// substituted (`url_path`).
    pub url_path: String,
    /// Query parameters in botocore's insertion order (list values
    /// expanded).
    pub query: Vec<(String, String)>,
    /// Headers in insertion order.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The expanded `endpoint.hostPrefix`, if any.
    pub host_prefix: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct HttpResponse {
    pub status: u16,
    /// Headers as received (original case and order).
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// `cli_timestamp_format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimestampFormat {
    #[default]
    Iso8601,
    Wire,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ParseOptions {
    pub timestamp_format: TimestampFormat,
}

/// A parsed error response (`parsed['Error']` plus modeled fields).
#[derive(Debug, Clone, Default)]
pub struct ServiceError {
    pub code: String,
    pub message: String,
    /// The whole parsed error response as botocore returns it
    /// (`{"Error": {...}, "ResponseMetadata": {...}, <modeled fields>}`).
    pub parsed: Value,
}

/// Serialize `params` (already validated) for `operation`.
pub fn serialize(
    model: &ServiceModel,
    operation: &Operation<'_>,
    params: &Value,
) -> Result<HttpRequest> {
    match model.protocol() {
        "json" => json::serialize(model, operation, params),
        "rest-json" => rest_json::serialize(model, operation, params),
        "smithy-rpc-v2-cbor" => cbor::serialize(model, operation, params),
        "query" => query::serialize(model, operation, params),
        "ec2" => ec2::serialize(model, operation, params),
        "rest-xml" => rest_xml::serialize(model, operation, params),
        other => bail!("unsupported protocol: {other}"),
    }
}

/// Parse a response as `Endpoint._do_get_response` does: the protocol
/// parser's result (`ResponseMetadata` holds `RequestId` when the service
/// sent one, then `HTTPStatusCode`, `HTTPHeaders`, and `RetryAttempts`;
/// its position in the dict is the protocol's: first for the rest
/// protocols, last for `json` and `smithy-rpc-v2-cbor`), with modeled
/// error fields merged for statuses of 300 and above.
pub fn parse(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<Value> {
    match model.protocol() {
        "json" => json::parse(model, operation, response, options),
        "rest-json" => rest_json::parse(model, operation, response, options),
        "smithy-rpc-v2-cbor" => cbor::parse(model, operation, response, options),
        "query" => query::parse(model, operation, response, options),
        "ec2" => ec2::parse(model, operation, response, options),
        "rest-xml" => rest_xml::parse(model, operation, response, options),
        other => bail!("unsupported protocol: {other}"),
    }
}

/// Parse an error response (status >= 300, or an S3-style 200 error).
pub fn parse_error(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<ServiceError> {
    match model.protocol() {
        "json" => json::parse_error(model, operation, response, options),
        "rest-json" => rest_json::parse_error(model, operation, response, options),
        "smithy-rpc-v2-cbor" => cbor::parse_error(model, operation, response, options),
        "query" => query::parse_error(model, operation, response, options),
        "ec2" => ec2::parse_error(model, operation, response, options),
        "rest-xml" => rest_xml::parse_error(model, operation, response, options),
        other => bail!("unsupported protocol: {other}"),
    }
}
