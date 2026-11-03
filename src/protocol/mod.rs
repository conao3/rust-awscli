//! Request serialization and response parsing for the botocore protocols
//! (`botocore/serialize.py`, `botocore/parsers.py`).

use anyhow::Result;

use crate::model::{Operation, ServiceModel};
use crate::value::Value;

pub mod ec2;
pub mod query;
pub mod rest_xml;
pub mod timestamp;
mod xml_common;

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
    let _ = (model, operation, params);
    todo!("protocol::serialize")
}

/// Parse a successful response. The result includes `ResponseMetadata`
/// first, as botocore's parsers produce it.
pub fn parse(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<Value> {
    let _ = (model, operation, response, options);
    todo!("protocol::parse")
}

/// Parse an error response (status >= 300, or an S3-style 200 error).
pub fn parse_error(
    model: &ServiceModel,
    operation: &Operation<'_>,
    response: &HttpResponse,
    options: &ParseOptions,
) -> Result<ServiceError> {
    let _ = (model, operation, response, options);
    todo!("protocol::parse_error")
}
